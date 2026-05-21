# IronPlayer Audio v0.2

## Overview
Adicionar reprodução de áudio estável ao IronPlayer para streams MPEG-TS multicast, cobrindo decode via FFmpeg, saída WASAPI, sincronismo básico A/V, telemetria operacional e identificação correta de codecs, PIDs e trilhas na UI. Sempre consulte o `docs\ironplayer-audio-prd-v0.2.md` antes de iniciar uma nova task.

## Tasks
- [x] Reabilitar o decode de áudio no pipeline FFmpeg, removendo o bypass temporário e produzindo `AudioFrame` válido sem depender de offsets frágeis de `AVFrame`
- [x] Implementar conversão robusta para PCM f32 interleaved com suporte a formatos planares/interleaved, `swresample`, mudanças de sample rate, canais e downmix estéreo quando necessário
- [x] Estabilizar a saída de áudio WASAPI com `cpal`, usando fila bounded, buffer de jitter configurável, recriação automática do dispositivo e tratamento de underrun/overrun sem bloquear vídeo ou UI
- [x] Adicionar suporte e validação para MP2, AAC-LC ADTS, AAC LATM/HE-AAC e AC-3, incluindo mapeamento por `stream_type` e descriptors DVB/ATSC em PMT
- [x] Atualizar métricas e UI para mostrar volume/mute, trilha ativa, codec, PID decimal/hex, idioma, sample rate, canais, nível de buffer, erros de áudio e estados operacionais
- [x] Escrever testes e fixtures de integração para codecs obrigatórios, descontinuidades de PTS/PCR, troca de serviço/trilha e identificação de PIDs sem regressão para vídeo, parsing TS ou UI

## Technical Details
Stack alvo: Rust, egui/eframe, FFmpeg 8.x distribuído via DLLs, `cpal` sobre WASAPI e canais bounded. O formato interno de áudio será PCM f32 interleaved; passthrough AC-3/E-AC-3 fica fora do escopo. A implementação deve preservar baixa latência, evitar panic com PES truncado ou codec não suportado, manter vídeo/UI isolados de falhas de áudio e exibir PIDs/`stream_type` em decimal e hexadecimal alinhados ao MediaInfo.