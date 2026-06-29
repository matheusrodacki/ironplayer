# Plano — Restaurar zero-copy de vídeo na UI Slint (IronPlayer)

## Context

A migração `egui → Slint` (commit `593fc44`) validou a UI visualmente, mas **regrediu a performance** de vídeo. O motivo: o caminho de render foi simplificado para **conversão YUV→RGBA na CPU**.

Hoje (`crates/ui-slint`):
- `video.rs` converte cada frame `VideoFrame` (planos YUV/NV12 da CPU) → `SharedPixelBuffer<Rgba8Pixel>` com aritmética inteira na CPU (`yuv420p_into` / `nv12_into`).
- Uma thread worker `slint-video-convert` roda a conversão; o `Poller` exibe via `Image::from_rgba8` (`lib.rs:107-196`).
- Slint usa o renderer **femtovg/OpenGL** (`Cargo.toml:15` → `renderer-femtovg`).
- Consequências: conversão CPU custa ~2 GB/s por 1080p60, sobe uma textura RGBA cheia por frame (4× a banda dos planos YUV), e **perde HDR/10-bit** (10-bit é reduzido a 8-bit; sem tone mapping PQ/HLG nem gamut BT.2020→709).

O caminho egui anterior já tinha a solução: **conversão YUV→RGB na GPU via shaders WGSL** (`crates/av/src/renderer.rs`, `yuv_to_rgb.wgsl`, `nv12_to_rgb.wgsl`), com upload de planos compactos (R8/Rg8/R16) e tone mapping HDR no fragment shader. Esse código **ainda existe mas está morto**: `crates/ui` (egui) saiu do workspace; `VideoRenderer` é `pub use`d em `av/lib.rs:45` mas não é consumido por ninguém.

Além disso há andaime explícito para o zero-copy real de hardware: `D3d11Texture::into_wgpu()` (`hw/d3d11_impl.rs:403`) é um stub documentado como "Fase C", e `D3d11Device::new()` já cria o device com `D3D11_CREATE_DEVICE_BGRA_SUPPORT` *"necessário para interop DXGI/wgpu (surface sharing)"* (`d3d11_impl.rs:161`) e guarda o `adapter_luid`.

**Objetivo:** restaurar o pipeline GPU e ir além — compartilhar a surface de decode D3D11 diretamente com a wgpu (escolha do usuário), eliminando também o download CPU dos frames de hardware. Manter um fallback CPU quando a wgpu não inicializar.

Slint 1.17 (resolvido em `Cargo.lock`) suporta o renderer **`renderer-femtovg-wgpu`** e a feature **`unstable-wgpu-29`**, que expõe `slint::wgpu_29` com `Image::try_from(wgpu::Texture)` (import zero-copy de textura) e `BackendSelector::require_wgpu_29(WGPUConfiguration::Manual{ instance, adapter, device, queue })`. Documentado em `slint-1.17.0/lib.rs:534-628`.

---

## Estratégia em duas fases

A entrega é faseada porque a **Fase 2 (interop D3D11↔wgpu) depende inteiramente da infra wgpu da Fase 1**, e a Fase 1 sozinha já recupera a maior parte da performance (elimina a conversão CPU) e restaura HDR/10-bit. Ambas estão no escopo; a Fase 1 é pré-requisito e entregável de forma independente.

Decisão transversal: usaremos **`WGPUConfiguration::Manual`** desde a Fase 1 (não o `::default()`), criando nós mesmos a `wgpu::Instance` com backend **DX12** no **adapter primário** (`EnumAdapters1(0)`, o mesmo que `D3d11Device::new` usa). Isso garante que o device da Slint, o device do pipeline de vídeo e o device D3D11 de decode fiquem no **mesmo GPU** — condição necessária para o compartilhamento de handle da Fase 2.

---

## Fase 1 — Pipeline GPU (shader YUV→RGB) + import de textura no Slint

Recupera o equivalente ao caminho egui: planos YUV/NV12 (na CPU) → textura GPU → shader WGSL → textura RGBA → `Image` no Slint. Sem conversão CPU. Restaura HDR e 10-bit.

### 1.1 `crates/av` — virar o renderer GPU "headless" (sem egui)

`crates/av/src/renderer.rs` e `crates/av/Cargo.toml`:
- **Bump `wgpu = "22"` → `wgpu = "29"`** e **remover `egui = "0.29"` + `egui-wgpu = "0.29"`** de `av/Cargo.toml` (mortos desde a saída de `crates/ui`). Atenção: wgpu 22→29 tem mudanças de API (descritores, `RenderPassDescriptor`, lifetimes de `RenderPass`, `TextureUsages`) a ajustar em `renderer.rs`.
- Reaproveitar do `renderer.rs` atual: os dois pipelines e seus uploads — `GpuRenderer`/`YuvRenderState` (YUV420P: 3 texturas R8/R16 + `yuv_to_rgb.wgsl`) e `NvRenderer`/`NvRenderState` (NV12/P010: R8/R16 + Rg8/Rg16 + `nv12_to_rgb.wgsl`), incluindo `upload_pending` (`renderer.rs:323`, `:807`) e a `YuvParams` UBO (colorspace/transfer/range, tone map PQ/HLG, gamut BT.2020→709).
- **Remover** o que é específico de egui: `paint_callback` (`:1378`), `texture_id` (`:1392`), `new_cpu`/`CpuRenderer` (`:1285`), e o `egui::PaintCallback`/`egui_wgpu::CallbackResources` em `prepare`/`paint` (`:537`, `:976`).
- **Substituir o alvo de render**: em vez de desenhar no render pass do egui, criar um método novo que renderiza para uma **textura offscreen `Rgba8Unorm`** própria, com usage `RENDER_ATTACHMENT | TEXTURE_BINDING` (o formato/usage exigidos pelo `Image::try_from` da Slint — ver `lib.rs:609`). Novo público, ex.:

  ```rust
  // Reutiliza upload_pending + pipeline + bind group; faz begin_render_pass
  // próprio contra a view da textura RGBA e draw(0..3) (triângulo fullscreen).
  pub fn render_to_texture(&mut self, frame: &VideoFrame, q: &wgpu::Queue) -> Result<wgpu::Texture, AvError>;
  ```
  Internamente faz match em `VideoFrame::Sw|Hw`, sobe os planos (`queue.write_texture`), roda o pass e devolve a textura. Manter um **pool de 2–3 texturas** dimensionadas ao vídeo (recriar em mudança de resolução) para evitar churn de alocação e permitir que a Slint amostre o frame N enquanto produzimos N+1.
- O `VideoRenderer` passa a guardar `Arc<wgpu::Device>` + `Arc<wgpu::Queue>` (a queue agora é usada de fato). Manter os contadores de banda (`gpu_upload_bytes_per_sec`) e labels de colorspace/range já existentes.

Arquivos: `crates/av/Cargo.toml`, `crates/av/src/renderer.rs`. Shaders reusados sem mudança: `crates/av/src/yuv_to_rgb.wgsl`, `crates/av/src/nv12_to_rgb.wgsl`.

### 1.2 `crates/ui-slint` — renderer wgpu + import de textura

`crates/ui-slint/Cargo.toml`:
- Trocar features da `slint`: remover `renderer-femtovg`, adicionar **`renderer-femtovg-wgpu`** e **`unstable-wgpu-29`** (manter `std`, `compat-1-2`, `backend-winit`). Idem `slint-build` (não muda).
- Usar os tipos wgpu via re-export `slint::wgpu_29::wgpu` (garante versão idêntica à da Slint e à do `av`).

`crates/ui-slint/src/lib.rs` (`run`):
- **Antes de `AppWindow::new()`**, criar a stack wgpu manual e instalar o backend:
  1. `wgpu::Instance` com `Backends::DX12`.
  2. Selecionar o `Adapter` cujo LUID == `D3d11Device::adapter_luid` (ou, na prática, o adapter primário — o mesmo `EnumAdapters1(0)`); logar e cair no fallback CPU se não houver adapter compatível.
  3. `request_device` (features incluindo `TEXTURE_FORMAT_NV12` para a Fase 2) → `device`, `queue`.
  4. `slint::BackendSelector::new().require_wgpu_29(WGPUConfiguration::Manual{ instance, adapter, device: device.clone(), queue: queue.clone() }).select()?`.
  5. Construir o `av::VideoRenderer` com `device`/`queue` (mesmos objetos).
- **Substituir o worker `slint-video-convert` + canais** pelo caminho GPU inline no tick (`Poller::tick`, `lib.rs:181-196`): quando `self.video.poll()` devolver um frame pronto, `let tex = renderer.render_to_texture(&frame, &queue)?; win.set_video_frame(Image::try_from(tex)?)`. O upload GPU é barato; roda na própria thread da UI (device/queue são `Send+Sync`; a submissão à queue é ordenada antes do frame da Slint). Remove a thread worker e a latência de canal.
- O `VideoState`/timing (`lib.rs:344-461`) e o `appwindow.slint` (`Image { source: root.video-frame; }`, `appwindow.slint:771`) **não mudam** — a propriedade `image` aceita tanto `Image::from_rgba8` (fallback) quanto `Image::try_from(texture)` (GPU).

### 1.3 Fallback CPU (escolha do usuário: manter)

- Se qualquer passo wgpu falhar (sem DX12, driver, LUID incompatível), logar e **manter o caminho atual**: `renderer-femtovg` (OpenGL) + thread `slint-video-convert` + `video::convert` + `Image::from_rgba8`.
- Implementação: encapsular a decisão num enum de modo de render no `Poller` (`Gpu { renderer, queue }` vs `Cpu { frame_tx, img_rx }`), escolhido no boot. `video.rs` permanece como está, só usado no ramo Cpu. **Nota:** a feature de renderer da Slint é escolhida em compile-time; para o fallback funcionar em runtime, compilar com **ambos** os renderers habilitados (`renderer-femtovg-wgpu` + `renderer-femtovg`) e deixar o `BackendSelector` cair no femtovg/GL quando `require_wgpu_29` falhar.

**Resultado da Fase 1:** zero conversão CPU para frames SW e HW; HDR/10-bit restaurados; 1 textura RGBA importada por frame (zero cópia CPU↔GPU no lado da exibição). Frames HW ainda passam pelo download de staging no decoder (resolvido na Fase 2).

---

## Fase 2 — Zero-copy real de hardware: surface D3D11 → wgpu (completar "Fase C")

Elimina o download CPU dos frames `VideoFrame::Hw`. Hoje `extract_nv12_planes` faz `CopySubresourceRegion` da array-texture do pool de decode para uma **staging texture CPU-readable** + `Map` + cópia para `Vec<u8>` (`d3d11_impl.rs:514+`, while-AVFrame-alive — `renderer.rs:1038`). Vamos manter a cópia GPU→GPU mas para uma **textura compartilhável**, e abri-la na wgpu (DX12) sem tocar a CPU.

Restrição técnica: a wgpu no Windows roda em **DX12**, não D3D11; e as texturas do pool de decode (`BIND_DECODER`, array) não são compartilháveis diretamente. O padrão é **shared NT handle + keyed mutex**.

### 2.1 Produtor (decoder, `crates/av/src/hw/d3d11_impl.rs`)
- Criar um **ring de ~3 texturas NV12 compartilhadas** (uma vez), `D3D11_USAGE_DEFAULT`, `BIND_SHADER_RESOURCE`, `MISC_SHARED_NTHANDLE | MISC_SHARED_KEYEDMUTEX`, no device de decode.
- Por frame HW: `IDXGIKeyedMutex::AcquireSync(key=0)` → `CopySubresourceRegion` do `array_slice` da textura do pool para a textura compartilhada do ring → `ReleaseSync(key=1)`. (Mesma cópia que hoje, mas o destino fica na GPU e é compartilhável; sem `Map`/CPU.)
- Expor o handle compartilhado (criado uma vez por textura via `IDXGIResource1::CreateSharedHandle`) + índice do ring no `VideoFrame::Hw` — substituindo o atual `NvPlanes` de CPU por uma referência `SharedNv12{ handle, key, width, height, ten_bit, metadata }`. Implementar isso completando `D3d11Texture::into_wgpu` (`d3d11_impl.rs:403`) ou um tipo análogo.
- **P010/10-bit:** a wgpu não expõe `TextureFormat::P010`. Frames HW 10-bit (HDR) **continuam pelo caminho de planos** (download → upload R16/Rg16 → shader) da Fase 1. Só **NV12 8-bit** segue o caminho compartilhado nesta fase.

### 2.2 Consumidor (wgpu/DX12, no `av::VideoRenderer`)
- Abrir o handle uma vez por textura do ring: via `device.as_hal::<wgpu::hal::api::Dx12, _, _>()` → `ID3D12Device::OpenSharedHandle` → `ID3D12Resource`; embrulhar com `wgpu::hal::dx12::Device::texture_from_raw(...)` + `wgpu::Device::create_texture_from_hal::<Dx12>(...)` declarando `TextureFormat::NV12`, usage `TEXTURE_BINDING`. Cachear o `wgpu::Texture` por slot do ring.
- Criar views de plano: `TextureAspect::Plane0` (R8Unorm, Y) e `Plane1` (Rg8Unorm, UV) e ligá-las ao pipeline `nv12_to_rgb.wgsl` já existente (binding tex_y/tex_uv).
- Sincronizar com keyed mutex em torno da submissão: `AcquireSync(key=1)` antes do pass, `ReleaseSync(key=0)` depois (devolve a textura ao produtor). Requer acesso hal à `IDXGIKeyedMutex` da resource aberta. Esta é a parte de maior risco/cuidado de ordenação.
- Saída idêntica à Fase 1: textura `Rgba8Unorm` → `Image::try_from` → `set_video_frame`.

### 2.3 Pré-condições e segurança
- LUID do device DX12 da wgpu **deve** bater com o `D3d11Device::adapter_luid`; se não, manter o caminho de planos (Fase 1) para HW. Logar a decisão.
- Manter o invariante crítico de lifetime (`renderer.rs:1038-1043`): a cópia para a textura compartilhada ocorre **na thread do decoder enquanto o `AVFrame` está vivo**; o keyed mutex impede o "zig-zag".

**Resultado da Fase 2:** frames HW NV12 8-bit vão decode(GPU) → cópia GPU→GPU → shader(GPU) → Slint, **sem nenhuma passagem pela CPU**.

---

## Arquivos a modificar (resumo)

| Arquivo | Mudança |
|---|---|
| `crates/av/Cargo.toml` | wgpu 22→29; remover egui/egui-wgpu |
| `crates/av/src/renderer.rs` | tirar egui; `render_to_texture()` para textura RGBA offscreen + pool; consumir frames compartilhados (Fase 2) |
| `crates/av/src/hw/d3d11_impl.rs` | ring de texturas NV12 compartilhadas + keyed mutex; implementar `into_wgpu` (Fase 2) |
| `crates/av/src/video_queue.rs` | variante `VideoFrame::Hw` carregando handle compartilhado em vez de (ou além de) `NvPlanes` (Fase 2) |
| `crates/ui-slint/Cargo.toml` | features slint: `renderer-femtovg-wgpu` + `unstable-wgpu-29` (manter `renderer-femtovg` p/ fallback) |
| `crates/ui-slint/src/lib.rs` | stack wgpu manual + `require_wgpu_29(Manual)`; render GPU inline no tick; enum de modo Gpu/Cpu; remover worker no modo Gpu |
| `crates/ui-slint/src/video.rs` | inalterado; usado só no fallback CPU |
| `crates/ui-slint/ui/appwindow.slint` | inalterado |

**Reuso-chave:** shaders `yuv_to_rgb.wgsl`/`nv12_to_rgb.wgsl`, `upload_pending`, `YuvParams`/tone mapping (`renderer.rs`); `D3d11Device::adapter_luid` + flag `BGRA_SUPPORT` (`d3d11_impl.rs:161`); stub `D3d11Texture::into_wgpu` (`d3d11_impl.rs:403`); timing `VideoState`/`VideoQueue` (`ui-slint/src/lib.rs:344`).

---

## Verificação

1. **Build/lint:** `cargo build` e `cargo clippy --workspace` (atenção à migração de API wgpu 22→29 em `renderer.rs`).
2. **Testes:** `cargo test --workspace` (timing/`VideoQueue` não deve regredir).
3. **SW path (Fase 1):** rodar `cargo run`, conectar a um TS H.264/HEVC 8-bit por software; confirmar vídeo correto e medir CPU/GPU. Comparar uso de CPU contra o `main` egui (deve cair drasticamente) e contra o estado Slint atual.
4. **HDR/10-bit:** stream 4K HDR HEVC 10-bit (PQ) — validar tone mapping e cor corretos (a UI Slint atual erra isso). Cruzar com o log de `current_colorspace_label`/`current_color_range_label`.
5. **HW path (Fase 2):** com D3D11VA ativo, confirmar `hw_decode_active` e que o caminho compartilhado é usado (log de LUID match + "shared NV12"); verificar ausência de "zig-zag"/tearing em movimento rápido; medir banda (`gpu_upload_bytes_per_sec` deve cair para ~0 no upload de planos HW 8-bit).
6. **Fallback:** forçar falha de wgpu (ex.: `WGPU_BACKEND` inválido / desabilitar GPU) e confirmar que cai no femtovg/GL + conversão CPU sem crashar.
7. **Telemetria de performance:** comparar frame pacing/dropped frames (instrumentação em `src/main.rs`) antes/depois.

## Riscos
- **Migração wgpu 22→29** em `renderer.rs` (mudanças de API não triviais).
- **Interop D3D11↔DX12 + keyed mutex** (Fase 2): ordenação de sincronização e lifetime são a parte mais delicada; mitigado por entregar a Fase 1 primeiro e manter o caminho de planos como rede de segurança.
- **`unstable-wgpu-29`** é feature instável da Slint (pode mudar em minor releases) — fixar `slint = "=1.17.x"`.
- **P010/HDR em HW** fica no caminho de planos (sem zero-copy de surface) por falta de suporte a P010 na wgpu.