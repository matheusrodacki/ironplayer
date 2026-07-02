# Debug de performance — vídeo picotado na UI Slint (1080i real)

> **Status:** Fixes aplicados e validados; 1 item em aberto (decisão de arquitetura)
> **Data:** 2026-07-02
> **Contexto:** Sessão de debug do sintoma "decode via GPU não funciona no
> feat/slint; no `main` funcionava perfeitamente". Relacionado a
> [zero-copy-plan.md](zero-copy-plan.md) (Fase 2) e a
> [STATE.md L-006/L-007](../../project/STATE.md#lições-aprendidas).

---

## Setup do teste (reproduzível)

Stream real do usuário via TSDuck replay em loop:

```sh
tsp -I file "C:\Users\Admin\Videos\Samples\GLOBO_RJ_S_LL_COPA.ts" --infinite \
    -P regulate -O ip 239.0.0.1:1234 --packet-burst 7
```

3 serviços SKY **1080i H.264 High@L4** (41 Mb/s): GLOBO RJ (svc 1, PID 1000),
SPORTV (svc 145, PID 450), GE TV (svc 1004, PID 1040). Máquina: Intel Arc
Graphics integrada (backend DX12), 1 único adapter físico.

Comparação A/B feita via `git worktree` do `main` (`71bcdbd`, ponto exato de
branch do `feat/slint`) rodando o mesmo stream em paralelo.

---

## Diagnóstico: duas causas distintas confundidas no sintoma original

### 1. HW→SW no deinterlace (comportamento por perfil)

Conteúdo 1080i real com perfil **Quality** (menu de contexto) → o decoder
migra de D3D11VA para software para aplicar o bwdif (`decoder.rs`).
Perfil **Performance** (padrão ao abrir o app) mantém D3D11VA e aplica
D3D11 Video Processor em GPU — **sem** migração HW→SW. Perfil **Desligado**
mantém HW sem deinterlace (campos visíveis, útil para debug zero-copy).

O controle de deinterlace é exclusivamente via menu de contexto Slint
(seção **Deinterlace**); não há mais `[decoder] deinterlace` no TOML.

### 2. Vídeo picotado no Slint (REGRESSÃO real, corrigida)

Comparação A/B no mesmo stream 1080i (decode SW nos dois, por causa do item 1):

| | `main` (egui) | `feat/slint` antes do fix |
|---|---|---|
| fps de vídeo exibido | 31.0 | 17.4 |
| frames tardios descartados | 0 | 3060 (atraso até 9.6 s) |

**Causas identificadas (por instrumentação de runtime, removida após o fix):**

1. **`ironstream.toml` desatualizado** na pasta do executável de teste:
   `skip_loop_filter=false` (o default real do código é `true`). Isolado,
   já custava ~6 fps. Não é bug de código — só ambiente de teste
   desatualizado; documentado aqui para não confundir futuros debugs.

2. **Um `pop()` da `VideoQueue` por tick do timer da UI** (`VideoState::poll`
   em `crates/ui-slint/src/lib.rs`). O timer roda a 16 ms nominal, mas o
   event loop real do Slint caía para ~24–30 Hz sob carga de render — cada
   tick só tirava 1 frame da fila, que ficava para trás até o
   `DROP_PTS` (100 ms) e era descartada em massa.

   **Fix:** `VideoState::poll()` agora drena toda a janela `Ready` da fila
   num único tick e devolve o frame **mais recente**; mantém a apresentação
   presa ao clock, não à cadência do timer/render.

3. **Batimento de fase timer × vsync**: um timer de 16 ms chamando
   `request_redraw()` por frame decodificado cria descompasso com o vsync —
   cada redraw "no meio" do período espera o próximo ciclo de vsync completo.

   **Fix:** o vídeo passou a ser dirigido pelo **próprio ciclo de render do
   Slint** — o poll acontece em `RenderingState::BeforeRendering` e o
   próximo redraw é agendado em `RenderingState::AfterRendering`
   (`crates/ui-slint/src/lib.rs`, `set_rendering_notifier`). Isso faz o
   loop rodar a 1 render por vsync, sem o descompasso do timer. O timer de
   16 ms continua existindo só para métricas/UI/CPU-fallback.

4. **femtovg redesenha a cena inteira a cada frame** (sem culling
   automático). Aplicado `cache-rendering-hint: true` nos blocos que mudam a
   ≤4 Hz (painel esquerdo de abas, top bar, status bar, `ChartCard`,
   `InfoCard` em `crates/ui-slint/ui/appwindow.slint`) — o femtovg materializa
   esses blocos como `Layer` cacheada e só re-renderiza quando o conteúdo
   muda de fato (`Property::set` já faz dirty-check por igualdade).

**Resultado após os 3 fixes de código** (fix 1 é ambiente, não código):
~29.5 fps em janela pequena (800×520), ~19–21 fps em 1400×900, **0 frames
tardios** em ambos.

---

## Pendência: fill-rate do femtovg na GPU integrada

Mesmo após os fixes, a cena completa em 1400×900 custa **~29 ms por frame**
(> 16.7 ms do vsync a 60 Hz), o que caps o vídeo em ~21 fps nessa resolução —
contra 31 fps constante do `main` (que usava shader wgpu próprio no caminho
quente do vídeo, sem redesenhar toda a UI a cada frame).

Ablação (via env vars de debug, removidas do código após a sessão):
esconder o painel esquerdo reduz a cena para ~22 ms; esconder o vídeo em si
tem impacto pequeno (~27 ms) — o custo dominante é **desenhar a UI ao redor
do vídeo a cada frame**, não o upload/shader do vídeo.

**Opções em aberto (decisão do usuário, não implementadas):**

1. **Skia** — renderer recomendado pela Slint para desktop, mas bloqueado por
   `+crt-static` (`.cargo/config.toml`; `skia-bindings` é `/MD`) **e** não
   compõe com `Image::try_from(wgpu::Texture)` — perderia o zero-copy do
   vídeo. Precisaria repensar o caminho de importação de textura.
2. **Software renderer** — tem partial rendering nativo (só o retângulo do
   vídeo é redesenhado, não a cena inteira), mas mata o pipeline GPU/zero-copy
   do vídeo (voltaria à conversão CPU YUV→RGBA por frame).
3. **Otimizar mais a cena femtovg** — o painel esquerdo cacheado ainda custa
   ~11 ms; suspeita não confirmada: o `ListView` de PIDs pode estar invalidando
   a layer mesmo sem mudança visível (candidato a próxima investigação).

**Invariante a não regredir:** o zero-copy GPU do vídeo (Fase 2) deve
continuar ativo e funcional; qualquer mudança de renderer que o comprometa
precisa de validação equivalente à desta sessão (contadores de
`SharedNvImporter`: `ok`/`fence_wait_skips`/`import_fail`) antes de merge.

---

## Checklist rápido em regressão futura de vídeo picotado no Slint

1. Confirmar perfil **Deinterlace** no menu de contexto (padrão: Performance)
   e se o stream é interlaced — HW→SW só é esperado em **Quality**.
2. Conferir que `VideoState::poll()` ainda drena a janela `Ready` inteira
   (não voltou a 1 pop por tick) — regressão reintroduziria o backlog de
   `DROP_PTS`.
3. Conferir que o poll de vídeo em modo GPU ainda roda em
   `RenderingState::BeforeRendering`/`AfterRendering` (render-loop contínuo),
   não num timer separado com `request_redraw` — regressão reintroduz
   batimento de fase com o vsync.
4. Conferir `cache-rendering-hint: true` nos blocos estáticos do
   `appwindow.slint` (painel esquerdo, top bar, status bar, `ChartCard`,
   `InfoCard`).
5. Medir fps de vídeo exibido e frames tardios (`video_queue::dropped_late`)
   contra a baseline desta sessão (~19–21 fps / 0 drops em 1400×900, 1080i,
   Intel Arc) antes de declarar uma regressão.

Refs: `crates/ui-slint/src/lib.rs` (`VideoState::poll`, `run`,
`set_rendering_notifier`, `GpuVideoBridge`), `crates/ui-slint/ui/appwindow.slint`
(`cache-rendering-hint`), `crates/av/src/video_queue.rs` (`DROP_PTS`,
`HOLD_PTS`, `pop_ready_with_resync`).
