# IronPlayer — UI Broadcast em Slint (substituindo egui)

## Context

A UI atual do IronPlayer usa **egui 0.29 + eframe + wgpu** (`crates/ui`, ~1.5k linhas).
O objetivo é modernizar o visual mirando o mockup anexado (modo **Broadcast**:
top bar de conexão, painel de PIDs/Tabelas/Serviços à esquerda, preview de vídeo +
gráficos de bitrate/PCR jitter no centro, Media Info + grade PSI/SI à direita, status bar).

Decisões já tomadas com o usuário:
- **Substituir egui** no binário principal `ironplayer` (não é binário separado).
- **Pipeline ao vivo**: a UI Slint consome os mesmos snapshots reais já produzidos em `src/main.rs`.
- **Vídeo real** dentro do Slint.

O foco é **reestilizar**, não criar recursos novos. Toda a lógica de backend
(`ts`, `av`, `net`) e o bootstrap do pipeline em `src/main.rs` permanecem; só troca a
camada de apresentação.

Fato-chave que viabiliza o vídeo: o decoder **sempre** entrega os planos na CPU —
`VideoFrame::Sw(YuvFrame{ planes: [Vec<u8>;3] })` (YUV420P) e
`VideoFrame::Hw(HwVideoFrame)` com NV12 já baixado para `Vec<u8>`
(`crates/av/src/decoder.rs:1060`, `crates/av/src/video_queue.rs:106,181`). Logo, dá para
converter YUV→RGBA na CPU e alimentar `slint::Image` via `SharedPixelBuffer`, sem
compartilhar device wgpu (caminho usado pelo exemplo oficial de FFmpeg do Slint).

## Abordagem

### Estrutura de crates
- Criar **`crates/ui-slint`** (nova crate de apresentação) e **remover `crates/ui`** (egui) do
  workspace e do binário, junto com `eframe`/`egui_plot` no `Cargo.toml` raiz.
- Antes de apagar `crates/ui`: **migrar** para `ui-slint` o que ainda é reutilizado — `AppCommand`,
  `AppState`/históricos e a função `update_metric_histories_if_new_snapshot`
  (`crates/ui/src/lib.rs`). Mover esses tipos para `ui-slint` (ou um módulo comum) e ajustar
  imports em `src/main.rs`.
- `src/main.rs` passa a chamar a UI Slint em vez de `eframe::run_native` (ver "Integração").
- `crates/av` fica **intacto**: continua usando `egui` internamente (`VideoRenderer`/`ColorImage`).
  A UI Slint **não** usa `VideoRenderer` — lê os planos direto de `VideoFrame`. Remover o egui de
  `av` está fora do escopo deste POC (a função de conversão CPU em `renderer.rs:608` serve de
  referência).

### Dependências (`crates/ui-slint/Cargo.toml`)
- `slint = "1"` (versão atual ~1.13; expõe `SharedPixelBuffer`/`Image` e charts via `Path`).
- `slint-build = "1"` em `[build-dependencies]`.
- Renderer: **femtovg** (`backend-winit`, `renderer-femtovg`) — mais leve e compatível com
  `+crt-static` (ver Riscos). Skia só se faltar nitidez em algo específico depois.
- `ts`, `av`, `net`, `crossbeam-channel`, `chrono` (mesmas do `crates/ui` atual).
- `build.rs`: `slint_build::compile("ui/appwindow.slint")`.

### Markup `.slint` (`crates/ui-slint/ui/`)
Reproduzir o layout do mockup Broadcast. Globals de tema (cores quase-pretas, accent laranja
~`#E8943A`, verde de status, fonte mono para dados técnicos). Componentes:
- `appwindow.slint` — janela, grid 3 colunas + top bar + status bar.
- `topbar.slint` — logo IRONPLAYER / "TS · STREAM ANALYZER", dropdown protocolo (UDP/TS),
  `LineEdit` de URL, botões **Conectar/Desconectar** (habilitação por estado de conexão),
  toggle Cinema/Broadcast (Broadcast ativo; Cinema é stub visual no POC).
- `pid_panel.slint` — abas PIDs/Tabelas/Serviços, filtro, total Mbps, e tabela de PIDs
  (PID dec+hex, badge TIPO colorido, LABEL, KBPS, CC, PKTS); realce de linha selecionada e
  linhas com `cc_errors > 0`. Dados via `ModelRc<PidRow>`.
- `video_panel.slint` — área de preview (`Image` ligada a property `video_frame`), overlay
  "AO VIVO" + timecode, barra "Reproduzindo · vol"; abaixo dois cards de gráfico.
- `charts.slint` — `BITRATE 60 S` (área) e `PCR JITTER` (linha com faixas ±) desenhados com
  `Path { commands: <string> }`; as strings de path são geradas em Rust a partir de
  `bitrate_history` / `pcr_history`.
- `mediainfo_panel.slint` — bloco VÍDEO (codec, perfil/nível, resolução, frame rate, aspecto,
  bitrate, PID) e ÁUDIO (codec, codec id, amostragem, canais, bitrate, idioma, PID); grade
  TABELAS PSI/SI com cards (PAT, PMT, SDT, NIT, EIT, TOT/TDT, SCTE-35…) e ponto de presença.
- `status_bar.slint` — conexão, Mbps, CC, buffer, resumo de áudio, HW decode/adapter.

### Glue Rust (`crates/ui-slint/src/lib.rs`)
- Função `run(...)` recebendo os mesmos handles que `IronPlayerApp::new` recebe hoje em
  `src/main.rs:1457` (`cmd_tx`, `snapshot_rx`, `conn_state`, `audio_status`, `selected_service`,
  `table_events_rx`, `video_frames_rx`, `pipeline_metrics`, `media_info`, `audio_clock`).
- Definir `struct`/`enum` Slint espelhando os dados de exibição (`PidRow`, `MediaInfoView`,
  `PsiTableCard`, `StatusView`). Mapear a partir de `ts::metrics::MetricsSnapshot` /
  `PidEntry`, `AudioStatusSnapshot`, `TablesSnapshot`, `MediaInfoCodecSnapshot`.
- **Polling** com `slint::Timer`:
  - ~60 Hz: drenar `video_frames_rx`, alimentar `VideoQueue`/`MasterClock` (reutilizar
    `crates/av/src/video_queue.rs`), converter o frame atual YUV→RGBA → `SharedPixelBuffer`
    → setar `video_frame`.
  - ~1 Hz (ou a cada snapshot novo): copiar `snapshot_rx`, atualizar models de PID, media info,
    tabelas, históricos e regenerar as strings de path dos gráficos. Reusar a lógica de janela
    de 60 s e dedup por timestamp de `crates/ui/src/lib.rs` (`update_metric_histories_if_new_snapshot`).
- **Conversão YUV→RGBA** (CPU): adaptar o caminho CPU já existente em
  `crates/av/src/renderer.rs:608-636` (hoje produz `egui::ColorImage`) para uma função que
  preencha `SharedPixelBuffer<Rgba8Pixel>` — ou usar `swscale` via `crates/av/src/ffi`. Tratar
  YUV420P e NV12; para P010/10-bit (Main10 do mockup) fazer down-shift para 8-bit (nota: refino
  HDR fica como follow-up).
- **Callbacks** Slint → `cmd_tx.try_send(AppCommand::…)`: Connect/Disconnect, SelectService,
  SelectAudio, SetVolume, SetHwAccel, SelectPid. O enum `AppCommand` já existe
  (`crates/ui/src/state.rs:420`) — mover/duplicar para `ui-slint` ou expô-lo de um lugar comum.

### Integração no binário (`src/main.rs`)
- Substituir o bloco `eframe::run_native(...)` (`src/main.rs:1423-1474`) por
  `ui_slint::run(...)`, passando os handles já construídos. Remover as deps `ui`, `eframe` e
  `egui_plot` do `Cargo.toml` raiz e do workspace. A validação de adapter wgpu vs D3D11 (Fase A,
  `src/main.rs:1433`) deixa de ser necessária no POC CPU — remover/comentar junto com o `cc`.

## Arquivos principais
- Novos: `crates/ui-slint/{Cargo.toml, build.rs, src/lib.rs}` + `crates/ui-slint/ui/*.slint`.
- Editar: `Cargo.toml` raiz (workspace members `ui`→`ui-slint`; remover `eframe`/`egui_plot`),
  `src/main.rs` (chamar a UI Slint).
- Migrar de `crates/ui` para `ui-slint` antes de remover: `AppCommand`, `AppState`/históricos
  (`crates/ui/src/state.rs`, `lib.rs`), depois **apagar `crates/ui`**.
- Reuso (sem editar): `crates/ts/src/metrics.rs` (snapshots), `crates/av/src/video_queue.rs`
  (`VideoFrame`, `VideoQueue`, `MasterClock`), `crates/av/src/renderer.rs:608` (conversão CPU
  como referência).

## Riscos / pontos de atenção
- **`+crt-static`** (`.cargo/config.toml`) + Skia: `skia-bindings` no MSVC pode conflitar com CRT
  estático e é pesado de compilar. Por isso começar com **femtovg**; se houver problema de build,
  avaliar remover o crt-static só para este alvo ou trocar de renderer.
- **10-bit/HDR** (Main10): conversão CPU inicial trata como 8-bit; fidelidade HDR é follow-up.
- **Custo CPU do readback/conversão**: aceitável a 1080p; em 4K pode pesar — otimização (ou
  caminho zero-copy via `slint::wgpu_28`) fica como evolução pós-POC.
- **egui em `crates/av`**: permanece como dependência de `av`; não é removida neste POC.

## Verificação
1. `cargo build` (workspace) sem erros; `cargo build --release`.
2. `cargo run --bin ironplayer` → conectar a `udp://@239.0.0.1:1234` (ou stream de teste UDP/TS).
3. Conferir, lado a lado com o mockup: top bar/conexão, tabela de PIDs povoando com bitrate/CC/PKTS,
   Media Info (vídeo+áudio), grade PSI/SI com presença, gráficos de bitrate (área) e PCR jitter
   (linha) animando, status bar, e **vídeo aparecendo** no preview.
4. `cargo test -p ts -p av` continuam verdes (backend intacto). Os testes de `crates/ui`
   relevantes (`AppCommand`, históricos) migram junto com o código para `ui-slint`.
5. Validar interações: Conectar/Desconectar, troca de serviço/áudio, volume.