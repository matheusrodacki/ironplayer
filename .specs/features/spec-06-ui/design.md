# TDD: `crates/ui` — Interface Principal egui

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-UI-001 a SPEC-UI-006
- **Fase:** Alpha v0.1 (UI-003) · Alpha v0.2 (UI-001/002) · Beta v0.3 (UI-004/005) · v1.0 (UI-006)

---

## Contexto e Problema

A UI é a camada de apresentação do IronPlayer. Precisa:

1. Exibir vídeo em tempo real em um painel dedicado
2. Mostrar análise de PIDs, tabelas PSI/SI e métricas de qualidade
3. Aceitar comandos do usuário sem bloquear o pipeline de TS

**Princípio central:** A UI nunca escreve estado diretamente. Ela lê `AppState` (imutável, snapshot) e envia `AppCommand` via canal MPSC para o backend. Isso garante que a thread egui nunca bloqueie o pipeline.

---

## Escopo

**In-scope:**
- Layout de 3 painéis + header + status bar (SPEC-UI-001)
- `AppState` e `AppCommand` — modelo de estado (SPEC-UI-002)
- `PidPanel` — tabela de PIDs com ordenação (SPEC-UI-003)
- `TablesPanel` — árvore PAT/PMT/NIT/SDT/EIT/TDT/BAT (SPEC-UI-004)
- `MetricsPanel` — gráficos de bitrate e jitter PCR, log de erros (SPEC-UI-005)
- `StatusBar` — estado de conexão, bitrate total, CC errors (SPEC-UI-006)
- Tema escuro/claro via `AppConfig`

**Out-of-scope:**
- Persistência de layout entre sessões
- Drag-and-drop de painéis
- Janela de preferências (rodmap)
- Internacionalização (apenas PT-BR / EN)

---

## Solução Técnica

### Estrutura do crate

```
crates/ui/
├── Cargo.toml
└── src/
    ├── lib.rs          # IronPlayerApp (eframe::App)
    ├── state.rs        # SPEC-UI-002: AppState, AppCommand, ConnectionState
    ├── panels/
    │   ├── mod.rs
    │   ├── video.rs    # SPEC-UI-001: VideoPanel
    │   ├── pid.rs      # SPEC-UI-003: PidPanel
    │   ├── tables.rs   # SPEC-UI-004: TablesPanel
    │   └── metrics.rs  # SPEC-UI-005: MetricsPanel
    └── status_bar.rs   # SPEC-UI-006: StatusBar
```

### Dependências (`Cargo.toml`)

```toml
[dependencies]
ts  = { path = "../ts" }
av  = { path = "../av" }
net = { path = "../net" }

eframe    = { version = "0.29", features = ["wgpu"] }
egui      = "0.29"
egui_plot = "0.29"
tokio     = { workspace = true }
crossbeam-channel = { workspace = true }
tracing   = { workspace = true }
```

---

## Modelo de Estado (`AppState` / `AppCommand`)

### `AppState` (SPEC-UI-002)

```rust
pub struct AppState {
    pub connection:       ConnectionState,
    pub metrics:          MetricsSnapshot,
    pub tables:           TablesSnapshot,
    pub selected_pid:     Option<Pid>,
    pub selected_service: Option<u16>,
    pub bitrate_history:  VecDeque<(std::time::Instant, f64)>,   // 60s
    pub pcr_history:      HashMap<Pid, VecDeque<PcrJitterRecord>>,
}

pub enum ConnectionState {
    Idle,
    Connecting { url: String },
    Connected   { url: String, since: std::time::Instant },
    Error       { url: String, reason: String },
}
```

### `AppCommand` (SPEC-UI-002)

```rust
pub enum AppCommand {
    Connect    { url: String, iface: Option<String> },
    Disconnect,
    SelectService { service_id: u16 },
    SelectPid     { pid: Pid },
    SetVolume     { volume: f32 },
    ResetErrors,
    ChangeTheme   { dark: bool },
}
```

**Fluxo de dados:**
```
Backend threads ──[watch::Receiver<MetricsSnapshot>]──► IronPlayerApp::update()
IronPlayerApp ──────────────────────────────────────────[AppCommand via mpsc]──► Backend
```

### `TablesSnapshot`

```rust
pub struct TablesSnapshot {
    pub pat:      Option<Pat>,
    pub pmts:     HashMap<u16, Pmt>,   // program_number → Pmt
    pub nit:      Option<Nit>,
    pub sdt:      Option<Sdt>,
    pub eit_pf:   HashMap<u16, (Option<EitEvent>, Option<EitEvent>)>, // svc → (atual, próximo)
    pub tdt:      Option<Tdt>,
    pub bat:      Option<Bat>,
}
```

---

## Layout dos Painéis (SPEC-UI-001)

```
┌─────────────────────────────────────────────────────────────────┐
│  [URL input ──────────────────────────] [Conectar] [Desconectar]│
├───────────────────────┬──────────────────┬──────────────────────┤
│   VideoPanel (40%)    │  AnalysisPanel   │   MetricsPanel (25%) │
│                       │     (35%)        │                      │
│   [vídeo/placeholder] │  Aba: PIDs       │   Bitrate graph      │
│                       │  Aba: Tables     │   PCR jitter graph   │
│   Codec / Res / FPS   │  Aba: Serviços   │   Error log          │
│   Volume slider       │                  │                      │
├───────────────────────┴──────────────────┴──────────────────────┤
│  StatusBar: estado · bitrate total · CC errors · TDT offset     │
└─────────────────────────────────────────────────────────────────┘
```

Implementado com `egui::SidePanel` + `egui::CentralPanel` + `egui::TopBottomPanel`.

---

## `PidPanel` (SPEC-UI-003)

**Colunas:**

| Coluna         | Tipo                | Ordenável         |
| -------------- | ------------------- | ----------------- |
| PID (hex)      | `String` ("0x0100") | sim               |
| Tipo           | `PidType` label     | sim               |
| Descrição      | `String`            | não               |
| Bitrate (kbps) | `f64`               | sim               |
| Pacotes        | `u64`               | sim               |
| Erros CC       | `u64`               | sim (padrão desc) |

**Comportamento (SPEC-UI-003a):**
- Linha com `cc_errors > 0`: fundo `egui::Color32` vermelho claro
- Linha `NullPacket`: cor de texto atenuada
- Clique → `AppCommand::SelectPid`
- Atualização incremental: sem rebuild completo a cada snapshot (usar `egui::Id` estável por PID)

---

## `TablesPanel` (SPEC-UI-004)

**Aba "Tables" — árvore collapsible (SPEC-UI-004a):**
Cada nó usa `egui::CollapsingHeader`. Estrutura:
```
▼ PAT  (v{version}, TS-ID: 0x{id})
  ├─ NIT PID: 0x{pid}
  ▼ Programa {n}  →  PMT PID: 0x{pid}
      PCR PID: 0x{pid}
      ├─ 0x{pid}  {stream_type_label}
      └─ ...
▼ NIT  (v{version}, network: "{name}")
  ...
▼ SDT  (v{version}, TS: 0x{id})
  ├─ Svc {id}: "{name}"  [{running_status}]
▼ EIT p/f (Svc {id})
  ├─ Atual: "{name}"  HH:MM–HH:MM
  └─ Próximo: ...
▼ TDT  →  {utc_time} UTC  (sistema: {offset}s)
▼ BAT  (bouquet: 0x{id} "{name}")
```

**Aba "Serviços" (SPEC-UI-004b):**
Grid com colunas: `ID | Nome | Tipo | EIT p/f | Status`. Clique duplo → `AppCommand::SelectService`.

---

## `MetricsPanel` (SPEC-UI-005)

**Gráfico de bitrate (SPEC-UI-005a):**
- `egui_plot::Plot` com dois `Line`: total (cinza) + PID selecionado (colorido)
- Eixo X: tempo, 60s de janela; eixo Y: kbps

**Gráfico de PCR jitter (SPEC-UI-005b):**
- Visível apenas para PIDs com PCR
- `HLine` tracejado em ±500 µs (vermelho)

**Log de erros (SPEC-UI-005c):**
- `egui::ScrollArea` + tabela: `Timestamp | Tipo | PID | Detalhe`
- Máximo 1000 entradas (configurable via `AppConfig`)
- Botão "Copiar (TSV)": `egui::Context::output().copied_text`
- Botão "Limpar": `AppCommand::ResetErrors`

---

## `StatusBar` (SPEC-UI-006)

```
[ ● Conectado  udp://@239.1.1.1:1234 ] [ 18.4 Mbps ] [ CC: 0 ] [ TDT: +0s ] [ v0.1.0 ]
[ ○ Desconectado ]
[ ⚠ Erro: {reason} ]
```

**SPEC-UI-006a:** indicador usa ícone Unicode + texto — nunca apenas cor. Acessível.

---

## Estratégia de Testes

A UI egui não tem testes de renderização automáticos em CI. Estratégia:

1. **Testes de estado:** `AppState` e `AppCommand` têm testes unitários sem egui.
2. **Testes de formatação:** funções de formatação de labels (hex PID, duração, bitrate) têm testes unitários.
3. **Smoke test de startup:** `eframe::run_native` com `--headless` não existe no egui 0.29 — aceitar validação manual.
4. **Screenshot tests:** roadmap pós-v1.0 usando `egui-kittest`.

```
spec_ui_002_app_state_default
spec_ui_003_pid_format_hex
spec_ui_003_pid_type_label
spec_ui_005_bitrate_history_window_60s
spec_ui_006_status_bar_text_connected
spec_ui_006_status_bar_text_error
```

---

## Considerações de Segurança

- URL digitada pelo usuário é validada via `StreamUrl::parse` antes de enviar `AppCommand::Connect` — nunca passada diretamente ao socket.
- `AppState` é lido como snapshot imutável; a UI não possui referência mutável ao estado do backend.
- "Copiar para área de transferência" usa a API do egui — sem acesso direto ao clipboard do OS.

---

## Riscos e Mitigações

| Risco                                                    | Mitigação                                                                           |
| -------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| egui rebuilds totais a 60 Hz com 200 PIDs causam jank    | Usar `egui::Id` estável por PID; evitar `String::new()` dentro de closures hot-path |
| `egui_plot` não suporta brush/zoom em v0.29              | Aceitar limitação em v0.1; adicionar panning em v1.0                                |
| Tema escuro/claro não persiste entre sessões             | Salvar em `ironstream.toml` via `AppConfig::ui.dark_theme`                          |
| wgpu backend pode exigir driver atualizado no Windows 10 | Documentar requisito mínimo: WDDM 2.0 (Windows 10 1607)                             |
