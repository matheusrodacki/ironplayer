# Spec: Media Info — análise de codecs e aba UI

- **Spec-IDs:** SPEC-MI-001 a SPEC-MI-006
- **Fase:** Beta v0.3+

---

## Requisitos

| ID | Requisito | Critério |
| --- | --- | --- |
| SPEC-MI-001 | Parsers de cabeçalho de codec em Rust puro (`ts::mediainfo`) | AVC/HEVC/MPEG-2 vídeo; MPEG-Audio/AAC/AC-3; zero FFI; `Result` em dados externos |
| SPEC-MI-002 | `StreamProbe` analisa todos os ES via canal `pes_probe` | Demux roteia `probe_pids`; probe auto-desregistra após parse estável |
| SPEC-MI-003 | `MediaInfoCodecSnapshot` publicado a ~1 Hz para UI | `Arc<RwLock<>>`; UI lê sem bloquear pipeline |
| SPEC-MI-004 | Aba "Media Info" no painel esquerdo | Blocos General / Video / Audio / Menu estilo MediaInfo |
| SPEC-MI-005 | Relatório mescla PSI/SI + bitrate + codec probe | ID, Menu ID, Codec ID, Format, idioma, bitrate, dimensões, colorimetria |
| SPEC-MI-006 | Botão copiar relatório texto | Clipboard com layout `chave : valor` alinhado |
