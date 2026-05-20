# IronPlayer — Pendências para Player Funcional

> **Status:** Em aberto  
> **Data:** 2026-05-20  
> **Contexto:** A aplicação já conecta em streams multicast UDP/RTP, parseia tabelas PSI/SI (PAT, PMT, NIT, SDT, EIT, TDT, BAT) e exibe métricas de bitrate e PCR jitter. O pipeline A/V está estruturalmente montado (demux → PES assembly → FFmpeg decode → VideoRenderer/AudioOutput). O que falta é fechar a lacuna entre "dados chegando" e "mídia tocando na tela".

---

## Pendência 1 — `SelectService` sem handler no backend

**Arquivo:** `src/main.rs` (thread `cmd-handler`, braço `_ => {}`)

O comando `AppCommand::SelectService { service_id }` é enviado pela UI quando o usuário dá duplo clique em um serviço na aba **Serviços**, mas o cmd-handler o descarta silenciosamente.

Consequências diretas:
- `AppState::selected_service` permanece `None` para sempre.
- Não há feedback visual de qual serviço está "em reprodução".
- O painel de vídeo exibe `[ sem stream ]` mesmo com stream ativo.

**O que implementar:**

1. Adicionar um `Arc<RwLock<Option<u16>>>` compartilhado entre o cmd-handler e o `IronPlayerApp`, análogo ao `connection_rx` já existente para `ConnectionState`.
2. No cmd-handler, ao receber `SelectService { service_id }`, gravar o valor nesse `RwLock`.
3. Em `IronPlayerApp::poll_snapshot()` (ou numa nova `poll_selected_service()`), ler o lock e atualizar `state.selected_service`.

---

## Pendência 2 — Roteamento A/V não filtra por serviço

**Arquivo:** `src/table_dispatcher.rs` (fn `dispatch_pmt`)

O `TableDispatcher` registra **todos** os PIDs A/V de **todas** as PMTs sem considerar o serviço selecionado. Em streams com programa único isso é inofensivo, mas em streams com múltiplos programas o decoder recebe PES de serviços distintos misturados.

**O que implementar:**

1. Compartilhar o snapshot de PMTs entre o `TableDispatcher` e o cmd-handler (ex.: `Arc<RwLock<HashMap<u16, Pmt>>>` indexado por `program_number` / `service_id`).
2. Adicionar variantes:
   - `DemuxCommand::DeregisterAvPid(Pid)` — remove PID A/V do `TsDemuxer`.
   - `PesCommand::DeregisterPid { pid: Pid }` — remove PID do `PesAssembler`.
3. No handler de `SelectService`:
   a. Encontrar a PMT do serviço via `service_id` → `program_number` → PMT.
   b. Enviar `DeregisterAvPid` / `DeregisterPid` para todos os PIDs do serviço anterior.
   c. Enviar `RegisterAvPid` / `RegisterPid` apenas para os PIDs do novo serviço.
4. Resetar o `FfmpegDecoder` ao trocar de serviço para evitar decodificação com contexto obsoleto.

---

## Pendência 3 — Seletor de serviço na UI (novo componente)

**Arquivo:** `crates/ui/src/panels/` (novo painel ou extensão do `VideoPanel`)

Atualmente não existe nenhum elemento de UI para o usuário escolher ativamente qual serviço deseja assistir. O duplo clique na aba Serviços funciona como workaround, mas não é intuitivo para uso cotidiano.

**O que implementar:**

Opção A — **Dropdown no cabeçalho:**  
Adicionar um `egui::ComboBox` na barra superior (ao lado de URL / Conectar) que liste os serviços disponíveis extraídos de `state.tables.sdt`. Ao selecionar um item, envia `AppCommand::SelectService { service_id }`.

Opção B — **Menu de contexto no `VideoPanel`:**  
Ao clicar com o botão direito na área de vídeo, exibir um popup com três submenus:

```
▶ Serviço  →  [• Service01]  [  Service02]  ...
   Áudio   →  [• PID 0x0101 AC-3]  [  PID 0x0102 AAC]  ...
   Legenda →  [  PID 0x0200 DVB Sub PT]  [  PID 0x0201 DVB Sub EN]  ...
```

| Submenu     | Fonte de dados                                                            | Comando enviado                                                       |
| ----------- | ------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| **Serviço** | `state.tables.sdt` + `state.tables.pat`                                   | `AppCommand::SelectService { service_id }`                            |
| **Áudio**   | streams com `stream_type` de áudio na PMT do serviço ativo                | `AppCommand::SelectAudioPid { pid }` (novo)                           |
| **Legenda** | streams DVB Subtitles (stream_type `0x06` + subtitling descriptor) na PMT | `AppCommand::SelectSubtitlePid { pid }` (novo — implementação futura) |

Item marcado com `•` indica seleção ativa. Itens desabilitados enquanto não conectado. Legendas DVB listadas mas marcadas como `[em breve]` até implementação completa (ver Roadmap).

Ambas as opções devem:
- Desabilitar o controle enquanto não conectado.
- Marcar visualmente o serviço atualmente em reprodução (`state.selected_service`).
- Usar o `service_name` da `Sdt` quando disponível; caso contrário, exibir `"Serviço 0x{id:04X}"`.

---

## Pendência 4 — Flag de configuração `auto_play_first_service`

**Arquivo:** `src/config.rs` (struct `PlayerConfig`) e `target/debug/ironstream.toml`

Para facilitar uso em cenários de monitoramento (onde só há um serviço ou sempre se quer o primeiro), deve haver uma flag que dispara `SelectService` automaticamente assim que a primeira PMT válida for processada, sem interação do usuário.

**O que implementar:**

1. Adicionar campo em `PlayerConfig`:
   ```toml
   # ironstream.toml
   [player]
   auto_play_first_service = true   # padrão: false
   ```
2. Passar o valor para o `TableDispatcher` (ou para o cmd-handler via canal de configuração).
3. Quando `auto_play_first_service = true` e nenhum serviço ainda estiver selecionado (`selected_service.is_none()`), ao receber a primeira PMT com streams A/V válidos, enviar automaticamente `SelectService { service_id }` para o próprio canal de comandos.
4. A flag **não** deve sobrescrever uma seleção manual já feita pelo usuário.

---

## Pendência 5 — Diagnóstico: frames de vídeo não chegam ao renderer

**Contexto:** O pipeline A/V está montado (`av-decode` thread existe, `VideoRenderer` inicializado), mas `VideoPanel` exibe `[ sem stream ]` mesmo com stream ativo e PIDs A/V registrados. Isso indica que nenhum `VideoFrame` chegou ao canal `video_frames`.

**Investigar:**

1. Executar com `RUST_LOG=debug` e filtrar pela thread `av-decode`:
   ```
   RUST_LOG=debug cargo run 2>&1 | grep "av-decode"
   ```
2. Verificar se `FfmpegDecoder::new()` retorna erro (a thread encerra silenciosamente drenando o canal vazio).
3. Verificar se os PIDs A/V chegam no `PesAssembler` — adicionar log temporário em `PesAssembler::push()` para confirmar que `pusi=true` (início de PES) é recebido.
4. Verificar se há backpressure no canal `pes_packets` (`CAP_PES_PACKETS = 256`) — o `BoundedSender` emite `warn` quando ≥ 90% cheio.

Possível causa conhecida: **race condition de ordem de registro** — o chunk com o primeiro PES do PID 0x0100 pode ter sido processado pelo `ts-demux` *antes* do `DemuxCommand::RegisterAvPid(0x0100)` chegar, fazendo o demuxer ignorar aquele PID até o próximo keyframe.

---

## Ordem sugerida de implementação

| #   | Pendência                                          | Esforço estimado | Impacto |
| --- | -------------------------------------------------- | ---------------- | ------- |
| 5   | Diagnóstico A/V — por que frames não chegam        | Baixo            | Crítico |
| 1   | Handler `SelectService` + `selected_service` na UI | Médio            | Alto    |
| 4   | Flag `auto_play_first_service`                     | Baixo            | Alto    |
| 3   | Seletor de serviço na UI (ComboBox)                | Médio            | Alto    |
| 2   | Roteamento A/V filtrado por serviço + Deregister   | Alto             | Médio*  |

\* Crítico apenas em streams com múltiplos serviços; inofensivo em streams simples.
