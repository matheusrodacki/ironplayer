# Tasks: spec-16-probe-video

> Gate: `cargo test -p ts -p av -p probe` verde + `cargo clippy -p ts -p av -p probe -- -D warnings`.

> **Status auditado em 2026-08-12:** nenhuma tarefa está concluída de ponta a
> ponta. Há contratos e detectores locais em `crates/probe/src/video.rs`, mas
> eles ainda não estão ligados ao decoder, `ProbeEngine`, sessão, relatório ou UI.

| # | Task | Status | Done when |
| --- | --- | --- | --- |
| T01 | Inventariar campos atuais de `MediaInfoCodecSnapshot` e ampliar metadados de vídeo/HDR necessários. | Parcial — tipos HDR existem, sem extração integrada dos parsers. | Parsers aceitam dados truncados com `Result`; snapshots distinguem ausente de inválido. |
| T02 | Criar contratos `VideoMetadataObservation` e `VideoFrameObservation`, canais bounded e métricas de drop. | Parcial — contratos/canal/teste de saturação existem, sem produtor e métrica de saúde integrados. | Produtor não bloqueia; teste de canal saturado contabiliza descarte local. |
| T03 | Implementar registro por serviço/PID, aplicabilidade e estados de disponibilidade. | Parcial — estados locais existem; PMT e pipeline não os alimentam. | PMT sem vídeo, codec não suportado, PES ausente e decoder indisponível são distinguíveis. |
| T04 | Implementar snapshots e eventos de metadados/alterações, incluindo HDR. | Parcial — deduplicação local existe, sem persistência. | Mudança de resolução/codec/HDR é deduplicada e persistida. |
| T05 | Implementar medidor de GOP e normalizador de erros de ES por codec. | Parcial — GOP e validação mínima existem; faltam parsers/fixtures por codec. | Fixtures MPEG-2/AVC/HEVC cobrem headers, sintaxe e GOP válido/mudança. |
| T06 | Implementar detector de freeze sobre luma reduzida. | Parcial — detector local sem integração ao motor/histerese. | Sequência estática abre/fecha pela duração/histerese; frame ausente não é freeze. |
| T07 | Implementar detector de black frame. | Parcial — detector local sem evento/histerese persistido. | Cobertura/luma/duração limiares têm testes de fronteira. |
| T08 | Implementar blockiness e blocky distortion atrás de perfil. | Parcial — há score simples de blockiness; falta blocky distortion e integração. | Método, score e `n/a` para formato inaplicável são verificáveis em fixture. |
| T09 | Implementar logo por ROI/referência versionada. | Pendente. | Referência ausente não alarma; presença/ausência/alteração geram estados corretos. |
| T10 | Integrar motor de checks, sessão, CSV/JSONL/relatório e abas Vídeo/Eventos. | Pendente. | Contexto serviço/PID/codec e `profile_version` chegam à UI e ao relatório. |
| T11 | Implementar orçamento/scheduler e degradação escalonada. | Pendente — a degradação genérica não agenda análise de vídeo. | Sob carga sintética, análise reduz custo sem bloquear pipeline A/V. |
| T12 | Executar gates e testes de regressão de Broadcast/Probe. | Parcial — testes e clippy passam; `cargo fmt --check` falha no workspace e não há regressão integrada. | Testes, clippy e `cargo fmt --check` verdes no escopo alterado. |
