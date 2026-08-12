# Tasks: Probe — checks e medições MPEG-TS

> **Status auditado em 2026-08-12:** T01 concluída. As demais tarefas abaixo
> registram a implementação parcial já presente ou o trabalho pendente; uma
> tarefa só foi marcada como concluída quando todos os seus critérios de
> aceite estavam atendidos.

## Ordem de implementação

- [x] **T01 — Caracterização e contratos atuais**
  - Mapear `TsEvent`, `PcrEvent`, `MetricsSnapshot` e os contadores consumidos pelo `ProbeEngine`.
  - Adicionar testes de caracterização para sync, CC, CRC, PCR e snapshot de serviços.
  - Done when: os testes descrevem o comportamento atual sem alterar o caminho do player.

- [ ] **T02 — Tipos de observação e ponte bounded**
  - Status: pendente. A Probe ainda deriva deltas de `MetricsSnapshot`; não há
    `TsProbeTick`, adaptador bounded nem `interval_incomplete`.
  - Criar `TsProbeTick`, evidências e adaptador em composição, todos documentados com SPEC-ID.
  - Definir capacidade, `try_send`, contador de drops e `interval_incomplete`.
  - Done when: canal cheio não bloqueia `TsDemuxer` e teste prova que ausência não é inferida em janela incompleta.

- [ ] **T03 — Sync e TEI**
  - Status: parcial. `SyncByteError`, `SyncLost` e TEI são distintos e chegam
    aos checks, mas a evidência completa do incidente não é persistida.
  - Separar `SyncByteError` de `SyncLost`; emitir evidência TEI sem descartar o restante do pipeline.
  - Registrar checks `sync_byte_error`, `ts_sync_loss` e `transport_error`.
  - Done when: SPEC-PROBE-TS-003/005 têm fixtures e eventos deduplicados.

- [ ] **T04 — Continuidade e CRC/PSI malformada**
  - Status: parcial. Há contadores por PID e classificação de PSI malformada,
    porém `expected`/`got`, tipo de pacote e contexto de tabela não chegam
    integralmente ao evento da Probe.
  - Preservar contexto CC e adicionar classificação segura de seção malformada.
  - Integrar deltas/evidências ao `CheckEngine` por PID/tabela.
  - Done when: SPEC-PROBE-TS-004/006 passam, inclusive CC 15→0, adaptation-only e null PID.

- [ ] **T05 — Inventário e temporizadores PAT/PMT/CAT**
  - Status: parcial. Grace window e ausência de PAT/PMT/CAT são avaliadas;
    snapshots versionados e eventos de mudança/validade ainda faltam.
  - Expor snapshots versionados de tabelas e implementar grace window/ausência/validade/mudança.
  - Modelar CAT, CA, scrambling, dados privados e DVB subtitle como inventário.
  - Done when: SPEC-PROBE-TS-007, 008 e 013 passam sem transformar presença de CA em falha.

- [ ] **T06 — Regras declarativas de PID**
  - Status: parcial. PIDs obrigatórios e proibidos são configuráveis, sem
    validação de associação por serviço nem estados explícitos completos.
  - Criar configuração para PIDs obrigatórios/proibidos e validação de associação de serviço.
  - Definir estados `not_applicable` e `unknown` para PIDs sem regra/classificação.
  - Done when: SPEC-PROBE-TS-009 tem testes por serviço e não acusa PID privado arbitrariamente.

- [ ] **T07 — PCR: dados e checks básicos**
  - Status: parcial. Jitter e descontinuidade por PID chegam ao `pcr_error`,
    mas repetição, frequência e rollups próprios não estão implementados.
  - Acumular repetição, jitter, descontinuidade e frequência por PID sem misturar relógio de rede.
  - Implementar `pcr_error` e séries/rollups associados.
  - Done when: SPEC-PROBE-TS-010 passa com tempo injetado e wrap-around de PCR.

- [ ] **T08 — Accuracy e drift PCR**
  - Status: pendente. Não há `pcr_accuracy_error`, qualidade de relógio,
    offset ou drift.
  - Implementar qualidade de clock, offset/drift e as guardas de amostras mínimas.
  - Manter `pcr_accuracy_error` indisponível sem clock/perfil válido.
  - Done when: SPEC-PROBE-TS-011 comprova os estados `active`, `ok` e `unavailable`.

- [ ] **T09 — Observação PES/PTS**
  - Status: parcial. Regressão de PTS em PID elementar é observada; ausência
    e intervalo inválido ainda não são avaliados com aplicabilidade completa.
  - Produzir fatos de PTS/PES de forma aditiva e avaliar regressão, intervalo e ausência apenas quando aplicável.
  - Done when: SPEC-PROBE-TS-012 passa e a ausência de decoder/PES não vira `pts_error`.

- [ ] **T10 — Bitrate e perfil avançado**
  - Status: parcial. Bitrate total/PID e null ratio existem, e T-STD/MGF/MGB
    são bloqueados sem modelo; faltam agregados persistidos por serviço/PID.
  - Publicar agregados multiplex/serviço/PID e null ratio para Probe.
  - Adicionar esquema validado de T-STD/MGF/MGB, inicialmente disabled/not-applicable.
  - Done when: SPEC-PROBE-TS-014/015 passam sem limites implícitos.

- [ ] **T11 — Correlação, persistência e UI**
  - Status: parcial. A correlação CC→RTP e o escopo de serviço/PID existem,
    mas faltam a aba Transport, evidências completas e exportação por escopo.
  - Adicionar `caused_by`, origem local/rede, referência normativa e evidência ao evento persistido.
  - Implementar a aba Transport, filtros de escopo e exportação.
  - Done when: SPEC-PROBE-TS-016/017 são verificáveis em sessão gravada e não duplicam incidente global por serviço.

- [ ] **T12 — Fixtures, regressão e gate**
  - Status: parcial. Os testes e clippy dos crates passaram na auditoria,
    mas faltam fixtures/cobertura de todos os cenários e `cargo fmt --check`
    falha no workspace atual.
  - Criar fixtures para sync, TEI, CC, CRC, PAT/PMT/CAT ausentes/mudando, PCR e PTS.
  - Rodar `cargo fmt --check`, `cargo test -p ts`, `cargo test -p probe`, `cargo clippy -p ts -- -D warnings` e `cargo clippy -p probe -- -D warnings`.
  - Done when: SPEC-PROBE-TS-018 está coberto e todos os gates passam.
