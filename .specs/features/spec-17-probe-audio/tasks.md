# Tasks: spec-17-probe-audio

> Gate: `cargo test -p ts -p av -p probe` verde + `cargo clippy -p ts -p av -p probe -- -D warnings`.

| # | Task | Done when |
| --- | --- | --- |
| T01 | Consolidar metadados de trilha dos descriptors/Media Info e definir `AudioMetadataObservation`. | Idioma, codec, sample rate e layout preservam ausente vs. inválido e geram mudanças deduplicadas. |
| T02 | Criar tap `AudioPcmObservation`, canal bounded e métrica de drops. | Decoder nunca bloqueia; teste de saturação torna a janela incompleta e incrementa saúde local. |
| T03 | Implementar registro por serviço/PID/trilha/canal e estados de presença/PES/decoder. | `audio_missing`, PES indisponível, decoder indisponível e silêncio são estados independentes. |
| T04 | Implementar RMS/peak por canal, nível e detector de silêncio. | Fixtures exercitam limiar, duração, janela incompleta e fechamento com histerese. |
| T05 | Implementar clipping e agregação por intervalo. | Contagem/duração/canal corretos; amostra isolada abaixo do mínimo não gera alarme. |
| T06 | Implementar true peak com modo/oversampling explícito. | dBTP e aplicabilidade têm fixtures e teste de limite. |
| T07 | Implementar loudness com `None` como default e um modo habilitado versionado. | Sem modo resulta `n/a`; modo habilitado produz valor atual/integrado/janela reprodutível. |
| T08 | Implementar pares estéreo esperados e detector de jitter noise por método suportado. | Mono não alarma sem perfil; par inativo e descontinuidade têm contexto correto. |
| T09 | Integrar motor de checks, sessão, relatório e abas Áudio/Eventos. | Eventos carregam serviço/PID/trilha/canal, valor, limite e `profile_version`. |
| T10 | Implementar scheduler/orçamento e degradação escalonada. | Carga sintética reduz checks caros sem afetar reprodução. |
| T11 | Executar fixtures de regressão e gates. | Testes, clippy e `cargo fmt --check` verdes no escopo alterado. |
