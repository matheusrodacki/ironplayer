# Tasks: spec-10-mediainfo

| # | Task | Done when |
| --- | --- | --- |
| T01 | `mediainfo/bitreader` + `model` | `cargo test -p ts spec_mi_` bitreader verde |
| T02 | Parsers AVC/HEVC/MPEG2/MPEG-Audio/AAC/AC-3 | Fixtures + testes por codec |
| T03 | `StreamProbe` + demux `probe_pids` | PES roteado; snapshot preenchido |
| T04 | Thread `media-probe` + wiring | Arc compartilhado com UI |
| T05 | `MediaInfoPanel` + merge report | Aba visível; copiar funciona |
| T06 | Descriptors TOT/NIT extras | General com country/timezone/orbital |
