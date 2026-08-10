# Design: Probe — checks de vídeo

## Arquitetura e ownership

`ts` continua dono de TS/PES, PSI/PMT e parsing seguro dos headers elementares.
`av` é dono da decodificação e converte quadros para uma observação compacta. `probe`
é dono da regra, estado, evento, série e retenção; `ui-slint` só consome snapshots
imutáveis. O wiring em `src/` conecta canais bounded — não há callback da UI para o
pipeline e não há segundo demux.

```
PMT + PES ──► ts::mediainfo ─┬─► VideoMetadataObservation ─┐
                              └─► av decoder ─► VideoFrameObservation ─┼─► probe::VideoAnalyzer
TS/PES/clock errors ──────────────────────────────────────────────────┘        │
                                                                           events/samples/snapshot
                                                                                   │
                                                                            session + UI
```

`VideoFrameObservation` não carrega o frame completo por padrão. Carrega uma miniatura
de luma em resolução limitada, PTS e metadados necessários ao cálculo. A imagem original
fica no caminho de player/snapshot existente. Isso limita cópia, memória e exposição de
payload em disco.

## Contratos propostos

```rust
/// SPEC-PROBE-VID-005
pub struct VideoFrameObservation {
    pub service_id: u16,
    pub pid: ts::Pid,
    pub pts_90khz: Option<u64>,
    pub width: u32,
    pub height: u32,
    pub luma_width: u16,
    pub luma_height: u16,
    pub luma: Box<[u8]>,
    pub received_at: std::time::Instant,
}

/// SPEC-PROBE-VID-002 · SPEC-PROBE-VID-003
pub struct VideoMetadataObservation {
    pub service_id: u16,
    pub pid: ts::Pid,
    pub codec: VideoCodec,
    pub resolution: Option<(u32, u32)>,
    pub aspect_ratio: Option<AspectRatio>,
    pub frame_rate: Option<Rate>,
    pub scan_type: Option<ScanType>,
    pub active_format: Option<ActiveFormat>,
    pub hdr: Option<HdrMetadata>,
}
```

Todos os campos de fonte externa são opcionais ou `Result`; valores de header inválidos
viram evidência de erro, nunca panic. Os canais de observação são bounded com `try_send`.
Um drop incrementa a saúde local da Probe e invalida somente a conclusão daquele intervalo.

## Detectores

- **Freeze:** compara miniaturas de luma após normalização simples; combina similaridade,
  PTS e tempo monotônico. Sem quadro novo é estado `analysis_stalled`, não freeze.
- **Black:** calcula cobertura de pixels abaixo de `black_luma_threshold`; requer cobertura
  e duração mínimas para abrir.
- **Blockiness/distorção:** o perfil seleciona método e mantém sua versão no evento. A
  implementação inicial mede energia de bordas em grade contra vizinhança; se o formato
  não permitir cálculo confiável, publica `n/a`, não score zero.
- **GOP/ES:** parsers de elementary stream em Rust alimentam estrutura/erros. FFmpeg pode
  acrescentar diagnóstico, mas um erro só vira `bitstream_error` com evidência do parser ou
  erro classificado/recorrente do decoder.
- **Logo:** compara somente ROI configurada com referência versionada; a referência não é
  persistida no relatório, apenas seu identificador/hash.

## Perfil e estados

Há uma seção por check em `[probe.checks]`; cada uma usa `enabled`, `threshold`, `window`,
`min_duration`, `clear_duration` e `severity` já definidos pelo motor. Parâmetros próprios
ficam em `[probe.video]` e por serviço, por exemplo `sample_fps`, redução de luma, ROI e
modo do detector. `enabled = false` é **desabilitado**; codec/metadata sem aplicabilidade é
**n/a**; incapacidade de decode é **indisponível**. Nenhum desses estados é OK.

O analisador chaveia por `(service_id, pid, check_id)`. Eventos carregam valor/limite/unidade,
codec, frame/GOP/PTS quando presentes, primeira/última ocorrência e `profile_version`.

## Limites de recurso

O scheduler dá prioridade ao player. Em Probe mode usa somente os serviços configurados;
em Broadcast a análise é opt-in. Sob orçamento excedido: baixa FPS/miniatura, suspende
blockiness/logo, registra `probe_degraded` e preserva metadados, disponibilidade e eventos
já abertos. Não descarta nem bloqueia PES/decoder de reprodução.
