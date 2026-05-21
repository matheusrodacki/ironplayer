# IronPlayer Audio PRD v0.2

MPEG-TS Multicast Player & Stream Analyzer

Product Requirements Document - Audio Roadmap

Versão 0.2 | Status: Rascunho para alinhamento

Data: 2026-05-21

Plataforma-alvo: Windows 10/11 x86-64

Stack: Rust · egui/eframe · FFmpeg 8.x · cpal/WASAPI

## 1. Contexto

O IronPlayer já validou o fluxo principal de vídeo MPEG-TS com recepção multicast, parsing TS próprio, seleção automática de serviço, decode via FFmpeg e renderização em tela. A próxima fase deve estabilizar o subsistema de áudio para transformar o player em uma ferramenta operacional completa para validação de streams broadcast e IPTV.

O foco deste PRD é áudio em MPEG-TS, cobrindo decodificação, saída WASAPI, sincronismo básico A/V, telemetria e compatibilidade com os codecs de áudio mais comuns em operação: MPEG-1 Layer II, AAC, AC-3 e HE-AAC.

## 2. Objetivos

### 2.1 Objetivo principal

Entregar reprodução de áudio estável, sincronizada e compatível com os principais codecs usados em Transport Stream, sem bloquear o pipeline de vídeo, parsing TS ou UI.

### 2.2 Objetivos de produto

- Reproduzir áudio do serviço selecionado automaticamente junto com o vídeo.
- Suportar MPEG-1 Layer II, AAC-LC, HE-AAC v1/v2 e AC-3.
- Exibir na UI codec, PID, taxa de amostragem, canais, bitrate e estado do buffer de áudio.
- Manter baixa latência e comportamento previsível sob perda de pacotes, troca de serviço e loop/descontinuidade de fonte.
- Preparar a arquitetura para E-AC-3, múltiplas trilhas, seleção manual de áudio e futuras opções de passthrough.

### 2.3 Fora do escopo desta fase

- Passthrough S/PDIF ou HDMI bitstream para AC-3/E-AC-3.
- Mixagem de múltiplos áudios simultâneos.
- Normalização loudness EBU R128/ATSC A/85.
- Legendas, closed captions ou áudio descrição.
- Suporte a DRM, scrambling ou streams criptografados.
- Sincronismo A/V com clock mestre completo de player profissional; esta fase exige sincronismo básico e baixa latência operacional.

## 3. Estado atual

### 3.1 Já existente

- O pipeline identifica codecs de áudio por `stream_type` no crate `av`.
- O decoder FFmpeg possui mapeamento para MP2, AAC, AC-3 e E-AC-3.
- Existe `AudioFrame` em PCM f32 interleaved.
- Existe `AudioOutput` com `cpal`/WASAPI e `AudioRingBuffer`.
- O canal `audio_frames` já está previsto no pipeline.

### 3.2 Bloqueios atuais

- O decode de áudio está temporariamente desabilitado porque a leitura direta de offsets de `AVFrame` para `sample_rate` e `ch_layout.nb_channels` não está compatível com a ABI real da libavutil-60 usada no ambiente atual.
- A conversão para PCM depende de offsets frágeis do `AVFrame`; o subsistema precisa migrar para uma abordagem mais robusta.
- Ainda não há seleção explícita de trilha de áudio na UI.
- O estado do buffer de áudio e underruns/overruns não aparecem em painel operacional.

## 4. Personas e casos de uso

| Persona                     | Necessidade                                                      | Critério de sucesso                                                           |
| --------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Engenheiro de broadcast     | Validar se o serviço multicast tem áudio audível e codec correto | Ouve o áudio e vê codec/PID/canais na UI em menos de 2 s após conectar        |
| QA de IPTV                  | Testar matriz de codecs e streams com perdas/descontinuidades    | Consegue identificar falha de codec, mute, underrun ou PID ausente sem crash  |
| Desenvolvedor de middleware | Inspecionar PMT e trilhas de áudio                               | Vê stream types, descriptors e trilha ativa por serviço                       |
| Operador de NOC             | Monitorar áudio em tempo real                                    | Recebe sinal visual de ausência de áudio, buffer vazio ou codec não suportado |

## 5. Requisitos funcionais

### RF-AUD-001 - Reabilitar decode de áudio

O sistema deve reabilitar o decode de áudio no `FfmpegDecoder` para PIDs registrados como áudio.

Critérios de aceite:

- PES de áudio não são descartados por bypass temporário.
- Frames decodificados geram `AudioFrame` com `sample_rate > 0`, `channels > 0` e amostras PCM válidas.
- Erros de decode são rate-limited por PID e não saturam o terminal.
- A ausência de áudio ou codec inválido não derruba vídeo nem UI.

### RF-AUD-002 - Compatibilidade MPEG-1 Layer II

O sistema deve reproduzir MPEG-1 Audio Layer II e MPEG-2 Audio Layer II em TS.

Mapeamento mínimo:

| Sinalização        | Interpretação |
| ------------------ | ------------- |
| `stream_type=0x03` | MPEG-1 Audio  |
| `stream_type=0x04` | MPEG-2 Audio  |

Critérios de aceite:

- Streams MP2 mono e estéreo reproduzem sem ruído ou canal invertido.
- Sample rates 32 kHz, 44.1 kHz e 48 kHz são aceitos.
- Bitrates CBR comuns entre 64 kbps e 384 kbps são aceitos.
- UI mostra `MPEG-1/2 Audio Layer II` ou nome equivalente.

### RF-AUD-003 - Compatibilidade AAC-LC

O sistema deve reproduzir AAC-LC em encapsulamentos comuns de MPEG-TS.

Mapeamento mínimo:

| Sinalização        | Interpretação |
| ------------------ | ------------- |
| `stream_type=0x0F` | AAC ADTS      |
| `stream_type=0x11` | AAC LATM/LOAS |

Critérios de aceite:

- AAC-LC ADTS estéreo 48 kHz reproduz corretamente.
- AAC-LC LATM reproduz quando suportado pela build FFmpeg distribuída.
- Troca entre serviços com AAC reinicia contexto de decode sem herdar estado antigo.
- UI distingue ADTS e LATM quando a sinalização permitir.

### RF-AUD-004 - Compatibilidade HE-AAC v1/v2

O sistema deve reproduzir HE-AAC v1 e HE-AAC v2 quando sinalizados como AAC no TS e suportados pelo FFmpeg.

Observações:

- HE-AAC geralmente aparece como AAC com SBR/PS no perfil interno, não como `stream_type` exclusivo.
- A detecção final do perfil deve vir do decoder/metadata, não apenas da PMT.
- HE-AAC pode expor taxa de saída diferente da taxa base codificada por causa de SBR.

Critérios de aceite:

- HE-AAC v1 estéreo reproduz na taxa de saída correta.
- HE-AAC v2 parametric stereo reproduz como estéreo quando o decoder entregar dois canais.
- UI mostra `HE-AAC` quando o perfil for detectável; caso contrário mostra `AAC` sem bloquear reprodução.
- Mudanças de sample rate após abertura do codec reiniciam a saída WASAPI sem crash.

### RF-AUD-005 - Compatibilidade AC-3

O sistema deve reproduzir AC-3/Dolby Digital em TS.

Mapeamento mínimo:

| Sinalização                            | Interpretação         |
| -------------------------------------- | --------------------- |
| `stream_type=0x81`                     | AC-3 ATSC             |
| `stream_type=0x06` + `AC-3_descriptor` | AC-3 DVB/private data |

Critérios de aceite:

- AC-3 2.0 reproduz como estéreo.
- AC-3 5.1 reproduz quando o dispositivo suportar o número de canais.
- Quando o dispositivo padrão não aceitar 5.1, o player deve fazer downmix para estéreo ou falhar com mensagem operacional clara; a preferência do produto é downmix para estéreo.
- UI mostra `AC-3 / Dolby Digital`, canais e sample rate.

### RF-AUD-006 - Sinalização via descriptors DVB/ATSC

O parser deve reconhecer codecs de áudio que usam `stream_type=0x06` com descriptors, especialmente em streams DVB.

Descriptors prioritários:

| Descriptor                      | Uso esperado               |
| ------------------------------- | -------------------------- |
| `0x0A` ISO 639 language         | Idioma da trilha           |
| `0x6A` AC-3 descriptor          | AC-3 em DVB private data   |
| `0x7A` Enhanced AC-3 descriptor | E-AC-3 futuro              |
| `0x7C` AAC descriptor           | AAC/HE-AAC quando presente |

Critérios de aceite:

- PMT com `stream_type=0x06` e descriptor AC-3 registra PID como AC-3.
- Idioma ISO 639 aparece na UI quando disponível.
- Descriptors desconhecidos não impedem registro de streams conhecidos.

### RF-AUD-007 - Saída WASAPI estável

O sistema deve abrir e manter saída de áudio usando `cpal` sobre WASAPI.

Critérios de aceite:

- Saída inicia automaticamente após o primeiro `AudioFrame` válido.
- Mudança de sample rate ou número de canais recria o `AudioOutput`.
- Erro ao abrir dispositivo não derruba o pipeline; a UI mostra estado `audio unavailable`.
- Callback de áudio nunca bloqueia o decoder nem a UI.

### RF-AUD-008 - Buffer de jitter de áudio

O sistema deve usar buffer de jitter configurável para absorver variações pequenas de chegada/decodificação.

Critérios de aceite:

- Buffer padrão: 100 ms.
- Configuração permitida: 50 ms, 100 ms, 200 ms e 500 ms.
- Underrun gera silêncio, contador e evento de diagnóstico.
- Overrun descarta áudio novo ou antigo conforme política documentada; a preferência do produto é manter baixa latência.

### RF-AUD-009 - Sincronismo básico A/V

O áudio deve respeitar PTS quando disponível e não deve se afastar perceptivelmente do vídeo em condições normais.

Critérios de aceite:

- Em stream estável, desvio percebido deve ficar abaixo de 150 ms.
- Em descontinuidade de PTS/PCR, o player pode resetar buffer de áudio e continuar.
- Troca de serviço limpa buffers e contextos de áudio.
- Loop artificial de TS curto pode gerar descontinuidade; o sistema deve recuperar sem crash.

### RF-AUD-010 - Controles de UI

A UI deve expor controles e estado de áudio suficientes para operação.

Critérios de aceite:

- Volume e mute acessíveis no player.
- Exibição de codec, PID, idioma, sample rate, canais e buffer level.
- Indicadores: `sem áudio`, `codec não suportado`, `buffer underrun`, `dispositivo indisponível`.
- Em MPTS com múltiplas trilhas, a primeira fase seleciona a primeira trilha compatível; fase posterior permite seleção manual.

## 6. Requisitos não funcionais

| Requisito       | Critério de aceite                                                                              |
| --------------- | ----------------------------------------------------------------------------------------------- |
| Baixa latência  | Áudio audível em até 2 s após seleção do serviço                                                |
| Estabilidade    | Nenhum panic com PES truncado, perda de TS packet ou codec não suportado                        |
| Isolamento      | Falha de áudio não interrompe vídeo, métricas ou parsing de tabelas                             |
| CPU             | Decode de áudio não deve adicionar mais de 5% de CPU em stream 1080p + áudio estéreo em release |
| Memória         | Buffer de áudio nominal até 500 ms sem crescimento não limitado                                 |
| Logs            | Erros repetitivos rate-limited por PID e tipo de erro                                           |
| Compatibilidade | Funcionar com FFmpeg distribuído em DLLs alongside, sem instalação global                       |

## 7. Arquitetura alvo

### 7.1 Fluxo esperado

```text
PMT/descriptors -> MediaCodec::Audio
PES assembler -> PesPacket
FfmpegDecoder -> AudioFrame PCM f32 interleaved
Audio queue bounded -> AudioOutput/WASAPI
Metrics/UI -> estado de codec, buffer e erros
```

### 7.2 Diretrizes técnicas

- Evitar depender de offsets instáveis de `AVFrame` para campos de áudio.
- Preferir APIs públicas do FFmpeg ou uma representação FFI completa e versionada quando acesso direto for inevitável.
- Normalizar saída para PCM f32 interleaved antes de entrar no `AudioOutput`.
- Usar `swresample` quando necessário para converter formato planar, sample format, layout ou downmix.
- Manter todos os canais bounded; nenhum áudio pode bloquear a UI.
- Tratar `AudioOutput` como recurso reiniciável por formato, não como singleton fixo.

## 8. Roadmap de fases

### Fase A0 - Recuperar áudio atual

Objetivo: voltar a produzir `AudioFrame` válido no stream AC-3 atual.

Entregas:

- Remover bypass temporário de áudio no decoder.
- Corrigir leitura de `sample_rate` e canais sem depender de offsets inválidos.
- Validar AC-3 48 kHz do stream de teste atual.
- Garantir que vídeo continue fluido com áudio ligado.

Done when:

- `cargo test -p av` passa.
- Stream atual reproduz vídeo e áudio em release.
- Logs não mostram erro `to_pcm_f32` com `sample_rate=0` ou `nb_channels=0`.

### Fase A1 - PCM robusto e WASAPI resiliente

Objetivo: estabilizar conversão e saída para qualquer formato PCM entregue pelo FFmpeg.

Entregas:

- Suporte a formatos planares e interleaved comuns: `fltp`, `s16p`, `s16`, `flt`.
- Conversão para f32 interleaved.
- Recriação automática do `AudioOutput` em mudança de formato.
- Contadores de underrun/overrun.

Done when:

- Testes unitários cobrem conversão planar estéreo e mono.
- Playback não estoura buffer em stream estável de 60 s.
- UI exibe formato de áudio ativo.

### Fase A2 - Matriz de codecs principais

Objetivo: validar compatibilidade MP2, AAC-LC, HE-AAC e AC-3.

Entregas:

- Fixtures ou streams curtos para cada codec.
- Registro correto por `stream_type` e descriptors.
- Relatório de compatibilidade no painel de serviços/PIDs.

Done when:

- MP2, AAC ADTS, AAC LATM, HE-AAC e AC-3 reproduzem em release.
- Cada codec possui pelo menos um teste ou fixture de integração documentado.
- Codec não suportado gera estado visível, não panic.

### Fase A3 - Sincronismo e baixa latência

Objetivo: reduzir drift audível e melhorar recuperação de descontinuidade.

Entregas:

- Política de clock mestre inicial.
- Reset de áudio em salto grande de PTS/PCR.
- Métrica de drift A/V estimada.
- Configuração de buffer de jitter.

Done when:

- Desvio percebido fica abaixo de 150 ms em stream estável.
- Loop/descontinuidade ressincroniza sem travar.
- UI mostra underrun/overrun/drift quando ocorrerem.

### Fase A4 - UX de áudio e múltiplas trilhas

Objetivo: tornar áudio operável em MPTS real.

Entregas:

- Seleção manual de trilha de áudio por serviço.
- Exibição de idioma ISO 639.
- Volume/mute persistidos em configuração.
- Estado visual de ausência de áudio por serviço.

Done when:

- Serviço com múltiplos áudios permite trocar trilha sem reconectar.
- Troca de trilha limpa decoder e buffer corretamente.
- UI mostra codec/idioma/PID da trilha ativa.

## 9. Matriz mínima de validação

| Codec           | Encapsulamento TS             | Sample rates        | Canais  | Obrigatório    |
| --------------- | ----------------------------- | ------------------- | ------- | -------------- |
| MPEG-1 Layer II | `0x03`                        | 32/44.1/48 kHz      | 1.0/2.0 | Sim            |
| MPEG-2 Layer II | `0x04`                        | 32/44.1/48 kHz      | 1.0/2.0 | Sim            |
| AAC-LC ADTS     | `0x0F`                        | 44.1/48 kHz         | 1.0/2.0 | Sim            |
| AAC-LC LATM     | `0x11`                        | 44.1/48 kHz         | 1.0/2.0 | Sim            |
| HE-AAC v1       | AAC + SBR                     | 24/48 kHz, 44.1 kHz | 2.0     | Sim            |
| HE-AAC v2       | AAC + SBR + PS                | 24/48 kHz, 44.1 kHz | 2.0     | Sim            |
| AC-3            | `0x81` ou `0x06` + descriptor | 48 kHz              | 2.0/5.1 | Sim            |
| E-AC-3          | `0x87` ou descriptor          | 48 kHz              | 2.0/5.1 | Futuro próximo |

## 10. Métricas de sucesso

- 100% dos codecs obrigatórios reproduzem em build release com fixtures conhecidas.
- Zero panic em fuzz/input truncado de PES de áudio.
- Sem crescimento contínuo de buffer após 10 minutos de reprodução.
- Menos de 1 underrun por minuto em rede local estável.
- Troca de serviço ou trilha conclui em até 2 s.
- Usuário consegue identificar codec, PID e estado de áudio sem olhar logs.

## 11. Riscos

| Risco                                   | Impacto                             | Mitigação                                                                 |
| --------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------- |
| ABI do FFmpeg muda entre builds         | Áudio quebra por offset inválido    | Usar API pública/struct FFI versionada e testes de smoke na inicialização |
| HE-AAC expõe sample rate dinâmico       | WASAPI toca em taxa errada ou falha | Recriar `AudioOutput` após primeiro frame real e em mudança de formato    |
| AC-3 5.1 não suportado pelo dispositivo | Sem áudio no desktop comum          | Downmix para estéreo via swresample                                       |
| LATM varia entre broadcasters           | AAC LATM falha em alguns streams    | Manter fixtures reais e fallback de erro operacional claro                |
| Buffer grande aumenta latência          | Player parece atrasado              | Perfis de buffer configuráveis e métrica de ocupação                      |

## 12. Decisões propostas

- D-AUD-001: O formato interno do IronPlayer para áudio será PCM f32 interleaved.
- D-AUD-002: A primeira versão sempre decodifica áudio para PCM; passthrough AC-3 fica fora do escopo.
- D-AUD-003: Quando o dispositivo não suportar o layout de canais, o fallback preferencial é downmix estéreo.
- D-AUD-004: Para live playback, baixa latência tem prioridade sobre reprodução perfeita de todos os samples.
- D-AUD-005: HE-AAC será tratado como perfil de AAC detectado após abertura do codec, não como codec separado na PMT.

## 13. Sequência recomendada de implementação

1. Corrigir introspecção de `AVFrame`/`AVCodecContext` para áudio.
2. Remover bypass temporário de áudio e validar AC-3 do stream atual.
3. Introduzir conversão robusta com `swresample` para PCM f32 interleaved.
4. Adicionar telemetria de buffer e erros de áudio na UI.
5. Expandir mapeamento por descriptors DVB para `stream_type=0x06`.
6. Montar fixtures de MP2, AAC ADTS, AAC LATM, HE-AAC e AC-3.
7. Implementar seleção manual de trilha de áudio.
