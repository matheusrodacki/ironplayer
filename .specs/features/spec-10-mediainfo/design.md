# Design: Media Info

## Tipos centrais (`ts::mediainfo::model`)

- `ElementaryCodecInfo` — campos opcionais por PID (vídeo/áudio)
- `MediaInfoCodecSnapshot` — `HashMap<Pid, ElementaryCodecInfo>`
- `StreamProbe` — remonta PES, despacha parser por `stream_type`

## Pipeline

```
TsDemuxer.probe_pids → pes_probe → media-probe thread → MediaInfoCodecSnapshot
TableDispatcher (PMT) → RegisterProbePid(all ES)
UI poll → AppState.media_info → MediaInfoPanel
```

## UI (`crates/ui/src/panels/mediainfo.rs`)

- `build_media_info_report(AppState)` — função pura
- `MediaInfoPanel::show` — ScrollArea + Grid + copiar
