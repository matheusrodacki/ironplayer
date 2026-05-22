# IronPlayer — Roadmap de Qualidade

> **Versão:** 0.3 (rascunho para revisão tópico-a-tópico)
> **Data:** 2026-05-21
> **Status:** Análise pré-execução — nenhum item abaixo foi implementado ainda.
> **Escopo:** consolidar lacunas técnicas identificadas em duas sessões de validação com TS reais SD/HD H.264 LATM via FFmpeg e 4K HEVC + E-AC-3 via TSDuck, e propor caminhos concretos de resolução, priorizados para chegar à Qualidade Profissional.

---

## 0. Objetivo de produto

Ser um **TS Analyzer + Player** gratuito, byte-exato e de baixa latência, capaz de:

1. Recepcionar UDP/RTP multicast (1316 B) e arquivos `.ts`/`.m2ts` sem perda de pacotes na varredura.
2. Decodificar PSI/SI **completa** segundo DVB (ETSI EN 300 468), ATSC (A/65) e ISDB-Tb (ABNT NBR 15603 / ARIB STD-B10).
3. Reproduzir A/V **sincronizado pelo PCR** com no máximo ±40 ms de drift sustentado entre áudio e vídeo (limiar EBU R 37).
4. Expor todas as métricas TR 101 290 níveis 1, 2 e 3 (sync_byte_error, CC_error, PCR_jitter, PCR_accuracy, PTS_error, CRC_error, transport_error, PID_error, etc.).
5. Renderizar UI a 60 Hz mesmo com vídeo 4K HEVC em decode CPU; jamais bloquear o pipeline de rede.

---

## 1. Diagnóstico: por que o vídeo "engasga" e o áudio toca livre

### 1.1 Pipeline atual (resumo)

```
net-recv ──► ts-demux ──► section-asm ──► table-disp ──► AppState (UI)
                  │
                  └──► pes-asm ──► av-decode ──► video_frames (cap=8) ──► UI repaint
                                              └──► audio_frames (cap=N) ──► AudioRingBuffer ──► WASAPI
```

Arquivos-chave:

- [crates/av/src/decoder.rs](crates/av/src/decoder.rs) — `FfmpegDecoder::decode` produz `DecodedFrame::{Video,Audio}` com PTS no domínio do FFmpeg (90 kHz convertido).
- [crates/av/src/audio.rs](crates/av/src/audio.rs#L57-L100) — `AudioRingBuffer` consome PCM e é drenado pela callback do cpal/WASAPI. **Áudio dita o tempo do mundo.**
- [crates/ui/src/lib.rs](crates/ui/src/lib.rs#L222-L260) — `poll_video_frames` drena até 8 frames do canal `video_frames`, **descarta tudo menos o último**, faz upload e o egui repaints. **Nenhum agendamento por PTS.**
- [src/channels.rs](src/channels.rs#L34-L35) — `CAP_VIDEO_FRAMES = 8`; o produtor usa `try_send_latest` (drop-oldest) e o consumidor também drop-oldest.

### 1.2 Por que o sintoma aparece

| Causa                                                                                                                 | Consequência observada                                                                                                                                  |
| --------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| O áudio é sincronizado com o clock WASAPI (independente).                                                             | Áudio nunca pausa nem desacelera.                                                                                                                       |
| Não existe **master clock**. Vídeo é "apresenta-quando-chega".                                                        | Frames atrasados pelo decoder HEVC são exibidos *late*, criando engasgo.                                                                                |
| Decoder HEVC software no FFmpeg sobrecarrega 1 thread; quando o burst de slices chega, o `decode()` atrasa 50–200 ms. | Os PESs ficam enfileirados em `pes_packets` enquanto `video_frames` esvazia → tela congela; depois 8 frames são despejados de uma vez → flicker rápido. |
| `try_send_latest` no produtor **descarta o oldest sem olhar PTS**.                                                    | Pode descartar B-frames já decodificados que ainda seriam exibidos antes de um I/P recém-chegado, causando reordering incorreto e saltos.               |
| O consumidor drena 8 frames e **mantém só `latest`**.                                                                 | Em pico, frames decodificados em 100 ms são reduzidos a 1, gerando "queda de fps" visível e dessincronia.                                               |
| `AudioRingBuffer` faz *catch-up* descartando samples antigos quando >2× o nível alvo.                                 | Áudio re-sincroniza sozinho; vídeo nunca tenta.                                                                                                         |
| Não há *resync* nem *re-clock* em discontinuidade PCR (DTS/PTS wrap, `discontinuity_indicator=1`).                    | Após troca de serviço ou wrap, vídeo pode ficar 0–95 h "no futuro".                                                                                     |

Em outras palavras: **hoje o IronPlayer não tem clock unificado**. O áudio toca pelo relógio da placa de som e o vídeo é apresentado "best-effort". Sob carga (4K HEVC, decodificação CPU, repaints da UI à 60 Hz), o vídeo é o elo fraco.

---

## 2. Sincronização A/V — proposta técnica

### 2.1 Modelo de clock

Adotar o modelo clássico de player profissional: **três relógios + um master selecionável**.

| Relógio             | Origem                                                       | Resolução                         | Uso                                   |
| ------------------- | ------------------------------------------------------------ | --------------------------------- | ------------------------------------- |
| `StreamClock` (PCR) | `PcrTracker` em [crates/ts/src/pcr.rs](crates/ts/src/pcr.rs) | 27 MHz nominal (90 kHz amostrado) | Referência teórica do mux.            |
| `AudioClock`        | Frames de áudio consumidos pelo WASAPI (samples → ms)        | sample-accurate                   | Padrão para *audio-mastered sync*.    |
| `WallClock`         | `Instant::now()` monotônico                                  | ns                                | Fallback quando não há PCR nem áudio. |

**Master padrão = `AudioClock`** (estratégia do VLC e do mpv quando há áudio). Vídeo é agendado contra ele; áudio nunca é descartado nem reamostrado nessa fase.

**Master = `WallClock`** quando o serviço não tem áudio (radio data, mosaicos, alguns canais de teste).

**Master = `StreamClock`** opcional para modo "monitoração de operadora" — útil para medir PCR_accuracy contra wall clock (TR 101 290 P1.5).

### 2.2 Pipeline com clock

```
av-decode ─► PtsQueue (heap por PTS, cap=N frames) ─► UI repaint
                              ▲
                              │ next_pts ≤ master_clock_now + lookahead?
                              │ sim → upload e marca apresentado
                              │ não → segura, repaint solicita revalidação
                              │ atrasado > threshold_drop → descarta e conta `late_dropped`
                              │ adiantado > threshold_hold → segura
```

### 2.3 Estruturas novas (sketch)

```rust
// crates/av/src/clock.rs (novo)
pub enum MasterClock {
    Audio(AudioClockHandle),  // samples_played / sample_rate
    Wall(Instant),            // start instant
    Pcr(PcrClockHandle),      // referência do mux
}

impl MasterClock {
    /// Tempo atual em PTS-units (90 kHz) ou microsegundos — escolher uma unidade canônica.
    pub fn now_pts90(&self) -> i64;
}

// crates/av/src/video_queue.rs (novo)
pub struct VideoQueue {
    inner: BinaryHeap<Reverse<PtsKeyed<VideoFrame>>>,
    capacity: usize,
    last_presented_pts: Option<i64>,
}

impl VideoQueue {
    /// Insere frame; descarta o mais novo se cheio, OU o mais antigo já vencido.
    pub fn push(&mut self, frame: VideoFrame);
    /// Retorna o próximo frame elegível para apresentação contra `now_pts`.
    /// Política:
    ///   - se `frame.pts < now_pts - DROP_THRESHOLD` → descarta e tenta o seguinte.
    ///   - se `frame.pts > now_pts + HOLD_THRESHOLD` → retorna None (segura).
    ///   - caso contrário → retorna `Some(frame)`.
    pub fn next_due(&mut self, now_pts: i64) -> Option<VideoFrame>;
}
```

### 2.4 Limiares iniciais (ajustáveis em runtime via UI debug)

| Parâmetro                                           | Valor inicial                   | Justificativa                               |
| --------------------------------------------------- | ------------------------------- | ------------------------------------------- |
| `HOLD_THRESHOLD` (segura frame por estar adiantado) | 20 ms                           | < 1 frame @ 50 fps                          |
| `DROP_THRESHOLD` (descarta por atrasado demais)     | 100 ms                          | EBU R 37 limit (vídeo após áudio)           |
| `RESYNC_THRESHOLD` (zera clock)                     | 500 ms                          | discontinuity ou stall pós-IDR              |
| `VideoQueue::capacity`                              | 16 frames (era 8)               | absorve I-frame burst de HEVC GOP 50        |
| `AudioRingBuffer` alvo                              | 80 ms (era 50/100/200/500 fixo) | reduzir latência sem underrun em jitter UDP |

### 2.5 Tratamento de descontinuidade PCR / PTS wrap

- Em [crates/ts/src/pcr.rs](crates/ts/src/pcr.rs) o `PcrTracker` já detecta `discontinuity_indicator`. Propagar `PcrEvent::Discontinuity` até o `MasterClock` para chamar `reset()` no clock e drenar `VideoQueue` e `AudioRingBuffer`.
- Wrap de 33-bit PTS (≈26.5 h): adicionar offset `wrap_count * 0x200000000` quando `prev_pts > new_pts + (1<<32)` (sinaliza wrap, não jump).

### 2.6 Métrica nova de qualidade

- `av_sync_offset_ms` (vídeo − áudio) — exibir no painel Métricas, gráfico de 60 s.
- `late_frames_dropped`, `early_frames_held`, `pts_discontinuities` — contadores em `MetricsSnapshot`.

### 2.7 Caminho de execução (fases)

1. **Fase A — instrumentação:** medir hoje, sem alterar nada. Adicionar logs do PTS de cada frame entregue à UI e do `audio_samples_played` no momento. Plotar drift por 60 s. Confirmar quantitativamente o sintoma.
2. **Fase B — `MasterClock::Audio`:** introduzir `AudioClockHandle` exposto pelo cpal callback (`output_samples_played: AtomicU64`).
3. **Fase C — `VideoQueue` por PTS:** substituir `try_send_latest` + drop-no-consumidor por uma fila por PTS com política de drop/hold.
4. **Fase D — descontinuidade & wrap:** integrar `PcrEvent::Discontinuity`.
5. **Fase E — telemetria de sync:** UI mostra offset em ms.

Critério de aceite: ao reproduzir SporTV 4K em CPU decode, drift sustentado entre áudio e vídeo deve permanecer dentro de **±40 ms** por >5 min, com `late_frames_dropped < 1 fps` em média.

---

## 3. Decodificação acelerada por GPU (D3D11VA / DXVA2 / NVDEC)

### 3.1 Por que importa

- HEVC 4K em CPU consome 200–400 % de 1 core Intel (Hyper-Threaded). Mesmo com `threads=auto`, ainda mantém o decoder na zona de risco para o sync.
- AV1 (já no roadmap operadoras) é inviável em CPU para 4K 60p.

### 3.2 Caminhos

| Backend                                | Disponibilidade Windows 10/11      | Custo de integração | Observações                                                                                     |
| -------------------------------------- | ---------------------------------- | ------------------- | ----------------------------------------------------------------------------------------------- |
| **D3D11VA** (FFmpeg `hwaccel d3d11va`) | Todas as GPUs com decoder hardware | Médio               | Mantém frame em `AV_PIX_FMT_D3D11`, ideal para zero-copy → wgpu (que já roda em D3D11 backend). |
| **DXVA2** (legado)                     | Win7+, GPUs antigas                | Médio               | Compatibilidade — útil só se quisermos suportar máquinas pré-D3D11.                             |
| **NVDEC/CUDA**                         | Apenas NVIDIA                      | Alto                | Adiciona dep CUDA — evitar como default.                                                        |
| **Intel QSV**                          | Apenas Intel iGPU/Arc              | Alto                | Idem.                                                                                           |

### 3.3 Proposta

- Adotar **D3D11VA como primário** (`hwaccel_device` igual ao `wgpu::Device` do egui_wgpu — *shared device*).
- Manter decode CPU como fallback automático em qualquer falha de criação de contexto hwaccel (laptops sem driver de aceleração, RDP, etc.).
- Adicionar opção `--hwaccel {auto,d3d11va,none}` em [src/config.rs](src/config.rs) e toggle na UI.

### 3.4 Zero-copy texture path

- Hoje `decoder.rs` chama `av_frame.to_rgb24()` → CPU `Vec<u8>` → `VideoRenderer::upload` → `wgpu::Texture` (cópia CPU→GPU 8.3 MB/frame @ 1080p ou 33 MB @ 4K).
- Com D3D11VA + wgpu compartilhando o mesmo `ID3D11Device`, dá para passar a `ID3D11Texture2D` direto como `wgpu::Texture` via `wgpu::hal::dx12::Texture::from_raw` (existe ponte no wgpu para D3D11 também).
- Ganho: elimina conversão YUV→RGB CPU e upload por frame.

### 3.5 Riscos

- Shared device: o `eframe::egui_wgpu` cria seu próprio `wgpu::Device`. Para compartilhar com FFmpeg precisamos:
  - Criar o `ID3D11Device` manualmente, **depois** passar para `wgpu::hal::dx12::Adapter::from_raw` ou usar a API `wgpu::Instance::create_surface_from_raw`.
  - Confirmar que `eframe` aceita `RenderState` externamente fornecido (existe `eframe::NativeOptions::renderer_setup_callback` em versões recentes; conferir 0.29).
- Formato: maioria dos decoders entrega `NV12` ou `P010` (HDR). Precisamos de shader YUV→RGB consciente de Rec.709/2020 e PQ/HLG para HDR.

---

## 4. PSI/SI — paridade com DVB / ATSC / ISDB-Tb

### 4.1 Estado atual confirmado

✓ PAT (0x00), PMT (0x02), NIT (0x40/0x41), SDT (0x42/0x46), BAT (0x4A), EIT P/F (0x4E/0x4F), TDT (0x70) — parseados e roteados em [src/table_dispatcher.rs](src/table_dispatcher.rs#L260-L275).

✓ Section assembly multi-section + `pointer_field` cruzando pacotes (correção registrada em `/memories/repo/psi-si-notes.md`).

✓ Service descriptor (0x48), NetworkName (0x40), BouquetName (0x47), SatelliteDelivery (0x43), CableDelivery (0x44), TerrestrialDelivery (0x5A), ServiceList (0x41), ShortEvent (0x4D).

### 4.2 Lacunas DVB (curto prazo)

| Item                                                    | Onde implementar                                                                                                 | Impacto                                                                                                            |
| ------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| **CAT (PID 0x0001, table_id 0x01)**                     | [crates/ts/src/demux.rs](crates/ts/src/demux.rs#L322) + dispatcher                                               | Obrigatório quando há scrambling (ISO 13818-1 §2.4.4.6). Permite descobrir CA_PIDs (EMM).                          |
| **NIT em PID dinâmico**                                 | [crates/ts/src/demux.rs](crates/ts/src/demux.rs) — adicionar `register_nit_pid()` análogo a `register_pmt_pid()` | PAT pode declarar `program_number=0 → PID≠0x0010` (comum em DTH). Hoje o NIT desse PID nunca chega ao section asm. |
| **TOT (table_id 0x73, PID 0x0014)**                     | dispatcher: novo arm `0x73 => dispatch_tot`                                                                      | Carrega `local_time_offset_descriptor` (timezone, DST). Mais comum que TDT puro no Brasil.                         |
| **EIT Schedule actual (0x50–0x5F) e other (0x60–0x6F)** | dispatcher: arms range                                                                                           | EPG real precisa de schedule, não só P/F.                                                                          |
| **DIT (0x7E), SIT (0x7F), RST (0x71)**                  | dispatcher                                                                                                       | Baixa prioridade — úteis em SPTS gravado.                                                                          |
| **Descritor `stream_identifier` (0x52)**                | [crates/ts/src/tables/descriptor.rs](crates/ts/src/tables/descriptor.rs)                                         | Component tag para casar EIT ↔ PMT.                                                                                |
| **Descritor `component` (0x50)**                        | descriptor                                                                                                       | Identifica codec/idioma de cada componente para EPG.                                                               |
| **Descritor `linkage` (0x4A)**                          | descriptor                                                                                                       | Pivô para serviço de emergência, mosaico, time-shifted.                                                            |
| **Descritor `parental_rating` (0x55)**                  | descriptor                                                                                                       | Classificação etária (Brasil: ABNT NBR 15603-2 estende).                                                           |
| **Descritor `CA_descriptor` (0x09)**                    | descriptor (já presente?)                                                                                        | Mapear EMM/ECM PIDs para diagnóstico CA.                                                                           |
| **Descritor `extended_event` (0x4E)**                   | descriptor + EIT merge                                                                                           | EPG completo de longa descrição.                                                                                   |
| **`Pat::parse` erro `expected` mal calculado**          | [crates/ts/src/tables/pat.rs](crates/ts/src/tables/pat.rs#L122-L128)                                             | Cosmético; corrigir cálculo.                                                                                       |

### 4.3 ATSC (A/65) — médio prazo

ATSC é raro em headends brasileiros, mas você mencionou possibilidade. Reservar como módulo opcional `crates/ts/src/tables/atsc/`:

- PID **0x1FFB** (Base PID) — roteamento dedicado no demux.
- **STT** (`0xCD`) — System Time Table (substitui TDT).
- **MGT** (`0xC7`) — Master Guide Table (tabela de tabelas).
- **TVCT** (`0xC8`) / **CVCT** (`0xC9`) — Virtual Channel Table (substitui SDT em ATSC).
- **RRT** (`0xCA`) — Rating Region Table.
- **EIT-k** (`0xCB`) — Event Information (índice por MGT).
- **ETT** (`0xCC`) — Extended Text Table.
- Strings em **multiple_string_structure** UTF-16/Huffman ATSC-1 — encoder separado em `dvb_string` ou novo `atsc_string`.

### 4.4 ISDB-Tb / ARIB STD-B10 — longo prazo

Operadoras de cabo recebem maioria dos terrestres como ISDB-Tb. Reservar `crates/ts/src/tables/isdb/`:

- **BIT** (0xC4, PID 0x0024) — Broadcaster Information Table.
- **NBIT** (0xC5/0xC6, PID 0x0025) — Network Board Information.
- **LDT** (0xC7, PID 0x0025) — Linked Description.
- **SDTT** (0xC3, PID 0x0023) — Software Download Trigger.
- **CDT** (0xC8, PID 0x0029) — Common Data (logos).
- Descritores específicos: `digital_copy_control` (0xC1), `audio_component` (0xC4), `ts_information` (0xCD), `partial_reception` (0xC0), `data_content` (0xC7), `event_group` (0xD6), `series` (0xD5), `system_management` (0xFE), `logo_transmission` (0xCF).
- **ARIB STD-B24 string encoding**: trabalho pesado. JIS X 0208 + escape sequences + controle (CSI). Sem isso, nomes ISDB-Tb saem como lixo. Plano: usar `encoding_rs` para JIS X 0208 + tabela manual de Mosaic A/B + caracteres de controle B24.

### 4.5 Cobertura TR 101 290

Hoje [crates/ts/src/metrics.rs](crates/ts/src/metrics.rs) cobre parte. Lista de itens TR-101 290 e estado:

| Indicador                         | Nível   | Status                                       |
| --------------------------------- | ------- | -------------------------------------------- |
| TS_sync_loss                      | 1.1     | ✓ via demux recover (verificar contagem)     |
| Sync_byte_error                   | 1.2     | ✓                                            |
| PAT_error / PAT_error_2           | 1.3     | parcial — precisa timeout 0.5 s              |
| Continuity_count_error            | 1.4     | ✓                                            |
| PMT_error / PMT_error_2           | 1.5     | parcial                                      |
| PID_error                         | 1.6     | falta — PIDs declarados em PMT mas sem fluxo |
| Transport_error                   | 2.1     | falta — bit TEI do header                    |
| CRC_error                         | 2.2     | parcial — confirmar agregação                |
| PCR_repetition_error (>40 ms)     | 2.3     | parcial via `PcrTracker`                     |
| PCR_discontinuity_indicator_error | 2.4     | parcial                                      |
| PCR_accuracy_error (±500 ns)      | 2.5     | falta — exige medição contra wall clock      |
| PTS_error (>700 ms)               | 2.6     | falta — agregar no decoder                   |
| CAT_error                         | 2.7     | falta — depende de §4.2 CAT                  |
| NIT_actual / NIT_other_error      | 3.1/3.2 | parcial                                      |
| SI_repetition_error               | 3.3     | falta                                        |
| Buffer_error (B5–B7)              | 3.4     | falta — exige T-STD                          |
| Unreferenced_PID                  | 3.5     | falta                                        |
| SDT_error                         | 3.6     | parcial                                      |
| EIT_error                         | 3.7     | parcial                                      |
| RST_error                         | 3.8     | falta                                        |
| TDT_error                         | 3.9     | parcial                                      |
| Empty_buffer_error                | 3.10    | falta                                        |
| Data_delay_error                  | 3.11    | falta                                        |

---

## 5. Rede e captura

### 5.1 RTP

- [crates/net/src/rtp.rs](crates/net/src/rtp.rs) já existe. Confirmar tratamento de:
  - Sequence number wrap (16-bit).
  - Reordering buffer (jitter buffer) — hoje pode estar em zero.
  - SSRC change → reset de pipeline.
  - Payload type 33 (MP2T) — único suportado, ok.

### 5.2 SMPTE 2022-1/2 FEC

Operadoras de IPTV usam frequentemente. Pendência — não está implementado. Reservar como crate `crates/net/src/fec.rs` futuramente.

### 5.3 SRT / RIST

Para distribuição moderna. Roadmap longo, mas conhecidos do mercado (StreamXpress já suporta). Manter no horizonte.

### 5.4 Captura para arquivo

Botão "Gravar" salvando `.ts` cru recebido (com timestamp do socket). Útil para reproduzir bugs.

---

## 6. UI / Analyzer

### 6.1 Funcional pendente (cf. [docs/player-funcional-pendencias.md](docs/player-funcional-pendencias.md))

- Hex view de seção PSI/SI selecionada.
- Árvore de descritores expansível (estilo Promax) com hex + decoded side-by-side.
- Lista de PIDs com sparkline de bitrate de 60 s por PID.
- Painel **TR 101 290** com semáforo verde/amarelo/vermelho por indicador.
- Painel **EPG** (lista de eventos correntes e próximos por serviço).
- Painel **CA** (CA_system_id, EMM/ECM PIDs, scrambled flag por PID).

### 6.2 Performance da UI

- Hoje a UI repinta a 60 Hz mesmo parado. Em vídeo 4K + lista de 1000 erros + sparkline, isso pesa.
- Mudar para repaint reativo (`ctx.request_repaint_after(...)`) baseado no próximo PTS due no `VideoQueue`.
- Limitar log de erros a "viewport visible" via `egui::ScrollArea::show_rows`.

### 6.3 Multiwindow / detach

MPC/VLC permitem destacar vídeo. egui suporta `Viewport` desde 0.27. Roadmap fácil.

### 6.4 Subtitle / closed caption

DVB Subtitling (0x59 descriptor), Teletext (0x56), CEA-608/708 em vídeo H.264 SEI / HEVC SEI. Totalmente ausente — não está no PRD ainda mas é diferencial vs concorrência free.

---

## 7. Áudio

### 7.1 Estado atual

- AAC-LATM, AAC-ADTS, AC-3, E-AC-3, MP2 suportados (cf. [crates/av/src/codec.rs](crates/av/src/codec.rs)).
- WASAPI shared mode via cpal.

### 7.2 Pendências

| Item                                                             | Caminho                                                                                                                             |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Pass-through bitstream (AC-3/E-AC-3 SPDIF) para receiver externo | WASAPI exclusive mode + `IAudioClient::Initialize` com `WAVE_FORMAT_EXTENSIBLE` + KSDATAFORMAT_SUBTYPE_IEC61937_DOLBY_DIGITAL_PLUS. |
| Downmix 5.1 → 2.0 configurável (ITU-R BS.775)                    | Implementar antes do ring buffer; hoje provavelmente cpal faz downmix automático ruim.                                              |
| Loudness LUFS (EBU R128)                                         | Crate `ebur128` Rust disponível.                                                                                                    |
| Visualização: VU meter, spectrogram                              | egui plot.                                                                                                                          |

---

## 8. Estabilidade / qualidade de código

| Item                                        | Local                                                                                                           | Risco                                 |
| ------------------------------------------- | --------------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| `unwrap`/`expect` em paths externos         | Auditar `crates/net/`, `crates/ts/` (zero panic em dados externos é regra do AGENTS.md).                        | Crash em stream malformado.           |
| Cobertura de teste de fuzz                  | Adicionar `cargo-fuzz` targets para `Pat::parse`, `Pmt::parse`, `Sdt::parse`, section assembler.                | Robustez vs streams hostis.           |
| Snapshot tests com TS reais                 | Salvar fixtures (Globo SC, SporTV 4K) recortadas em `crates/ts/tests/fixtures/real/` e testar parse end-to-end. | Regressão.                            |
| Limite de logs `tracing::warn` em hot paths | Já há rate-limit em `av-decode`. Verificar net-recv.                                                            | Saturação de stderr.                  |
| `bcd32_to_u64` em `descriptor.rs`           | Validar que é nibble-a-nibble (BCD verdadeiro), não inteiro decimal.                                            | Frequências de satélite/cabo erradas. |

---

## 9. Roadmap priorizado (proposta)

### Sprint 1 — Sync A/V (resolve o engasgo)

- §2.7 Fase A → E (master clock = áudio, `VideoQueue` por PTS).
- §4.2 CAT + NIT dinâmico + TOT (quick wins).
- §8 fixtures reais.

### Sprint 2 — GPU decode

- §3 D3D11VA shared device + fallback CPU.
- §3.4 zero-copy YUV→RGB (NV12).

### Sprint 3 — TR 101 290 completo

- §4.5 indicadores faltantes.
- §6.1 painel semáforo.
- §5.1 jitter buffer RTP completo.

### Sprint 4 — EPG e PSI/SI ampliados

- §4.2 EIT Schedule, descritores faltantes.
- §6.1 painel EPG.

### Sprint 5 — ATSC (opcional)

- §4.3.

### Sprint 6 — ISDB-Tb (longo prazo)

- §4.4 incluindo ARIB STD-B24.

### Sprint 7 — Pass-through e CC/Subs

- §7.2, §6.4.

---

## 10. Critérios de aceitação globais ("Dektec-grade")

1. Reproduz SporTV 4K HEVC + E-AC-3 com **<40 ms** de drift A/V sustentado por 1 h.
2. Detecta e exibe TR 101 290 P1/P2/P3 com taxa de falso-positivo <1 %/h em stream limpo.
3. CPU <60 % em laptop i5 8ª geração reproduzindo 4K HEVC (com hwaccel).
4. UI repinta a 60 fps sem stutter perceptível durante varredura de 60 s de bitrate.
5. Zero panics em fuzz de 10⁶ pacotes TS aleatórios.
6. PSI/SI byte-exato vs Promax TS Analyzer para 10 streams de teste (Globo SC, SporTV 4K, ESPN HD, GloboNews, BandSports, 2 DTH Ku, 2 ISDB-Tb terrestre, 1 ATSC).
7. Captura UDP/RTP a 80 Mbps sem perda de pacote em 10 min (verificar `packets_received == expected_by_cc`).

---

## 11. Decisões abertas

- **D-008 — Master clock default:** áudio ou wall? Proposta: áudio quando houver, senão wall.
- **D-009 — Unidade de PTS interna:** manter 90 kHz (compatível com FFmpeg) ou converter tudo para `Duration` (microsegundos)? Proposta: i64 90-kHz para evitar arredondamento e dor de cabeça com wrap.
- **D-010 — wgpu device shared com FFmpeg?** Adicionar branch experimental antes do commit principal.
- **D-011 — ARIB STD-B24 nativo vs binding C?** Implementar Rust puro (ETSI EN 300 468 §A já implementado dá meio caminho) ou wrap `libaribcaption`? Proposta: Rust puro, evita +1 DLL.
- **D-012 — TR 101 290 como crate separado ou parte de `ts::metrics`?** Proposta: parte de `ts::metrics`, com módulo `ts::metrics::tr101290`.

---

## 12. Referências

- ISO/IEC 13818-1 (MPEG-2 TS)
- ETSI EN 300 468 v1.16.1 (SI for DVB)
- ETSI TR 101 290 v1.4.1 (Measurement guidelines)
- ATSC A/65:2013 (PSIP)
- ABNT NBR 15603 (SI ISDB-Tb), ARIB STD-B10, ARIB STD-B24
- EBU R 37, EBU R 128
- SMPTE 2022-1/2/7
- ITU-R BS.775 (downmix)
