# Design: Probe — checks de áudio

## Arquitetura e ownership

O demux e a classificação de PID continuam em `ts`; `av` decodifica o PES e emite um
tap PCM para a análise. `probe` mede, mantém estado por trilha/canal e usa seu motor de
checks/perfis para persistir eventos. O tap não é a fila de reprodução e não abre `cpal`.

```
PMT/descriptors ─► ts::mediainfo ─► AudioMetadataObservation ─┐
PES ─► av decoder ─► AudioPcmObservation ───────────────────────┼─► probe::AudioAnalyzer
TS/PES/discontinuidades ────────────────────────────────────────┘       │
                                                                     events/samples/UI
```

## Contratos propostos

```rust
/// SPEC-PROBE-AUD-004
pub struct AudioPcmObservation {
    pub service_id: u16,
    pub pid: ts::Pid,
    pub track_id: u8,
    pub pts_90khz: Option<u64>,
    pub sample_rate_hz: u32,
    pub channel_layout: ChannelLayout,
    pub frames: u32,
    pub interleaved_f32: Box<[f32]>,
    pub received_at: std::time::Instant,
}

/// SPEC-PROBE-AUD-002
pub struct AudioMetadataObservation {
    pub service_id: u16,
    pub pid: ts::Pid,
    pub track_id: u8,
    pub language: Option<String>,
    pub codec: AudioCodec,
    pub sample_rate_hz: Option<u32>,
    pub channel_layout: Option<ChannelLayout>,
}
```

Os blocos usam PCM `f32` normalizado e são enviados por canal bounded com `try_send`.
O produtor não espera o analisador. Ao haver drop, o `AudioAnalyzer` marca a janela como
incompleta e não conclui silêncio/jitter nela; a saúde local da Probe recebe o contador.

## Medidores e estados

- **Nível/silêncio/clipping:** acumulam por canal. Silêncio só usa janelas sem lacunas e
  PCM válido; clipping mantém contador e intervalo contínuo.
- **True peak:** o perfil informa oversampling e unidade dBTP; se a implementação não
  suportar o modo pedido, publica `n/a` em vez de estimar sem declarar.
- **Loudness:** enum `None | EbuR128 | AtscA85 | Custom`. `None` é default e não inicia
  medidor. Uma implementação habilitada deve carregar modo, gating e unidade no evento.
- **Par estéreo:** o perfil declara quais pares são esperados; a regra compara atividade e
  presença de cada membro, nunca deduz obrigação de estéreo de uma trilha mono.
- **Jitter noise:** recebe descontinuidades/PTS e medições PCM. Sem método aplicável ou
  sem relógio confiável, fica `n/a`.

Chaves de estado são `(service_id, pid, track_id, channel, check_id)`. Estados possíveis:
`ok`, `active`, `disabled`, `not_applicable` e `unavailable`; os três últimos não devem
aparecer como OK na grade. Todo evento contém unidade, measured/threshold, versão do perfil,
codec, trilha e canal quando existentes.

## Perfil e orçamento

`[probe.audio]` define taxa máxima de análise, duração de blocos, pares esperados e modo de
loudness. `[probe.checks.<id>]` usa o mecanismo comum de limiar, janela, debounce,
histerese e severidade. Sob carga, o scheduler preserva metadados e disponibilidade, depois
nível/silêncio; loudness, true peak com alto oversampling e jitter noise são reduzidos ou
suspensos com evento `probe_degraded`. O áudio do player nunca é bloqueado ou descartado por
causa da Probe.
