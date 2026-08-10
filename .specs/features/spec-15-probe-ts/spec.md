# Spec: Probe — checks e medições MPEG-TS

- **Spec-IDs:** SPEC-PROBE-TS-001 … SPEC-PROBE-TS-018
- **Crates:** `crates/ts` · `crates/probe` · `src` · `crates/ui-slint`
- **Fase:** v0.4 — Probe, camada 2 (Transport)
- **Dependências:** [spec-13](../spec-13-probe-mode/spec.md) · [spec-14](../spec-14-probe-ip/spec.md)
- **Origem:** `.notVersioned/Requisitos_Probe_MPEGTS_v0.4.docx`, prints 1–3

---

## 1. Objetivo e fronteiras

Esta feature promove os eventos e contadores já produzidos pelo demux MPEG-TS a
checks versionados da Probe, com estado, escopo, histórico e evidência. Ela
responde se o transporte, uma tabela PSI/SI, um serviço ou PID está íntegro,
sem tratar RTP/IP como perda TS e sem decodificar conteúdo A/V.

O ponto de partida existente é `TsEvent::{SyncLost, CcError, CrcError}` e
`PcrEvent::{Jitter, Discontinuity}`, além de `MetricsSnapshot`. A promoção não
deve trocar o parser Rust por FFmpeg, nem alterar o caminho de reprodução.

### Fora do escopo

- Checks IP/RTP/FEC: spec-14.
- Disponibilidade, qualidade, headers e PTS/DTS de elementary stream: specs
  16 e 17, exceto a evidência de transporte necessária para correlação.
- Descriptografia, validação editorial de legenda e interpretação do conteúdo
  de PID privado.
- Buffer analysis e T-STD ativos por padrão. Só podem ser ativados após o
  perfil operacional fornecer modelo, unidades, limites e dados de entrada.
- Semântica dos perfis MGF/MGB além de registrar sua configuração e marcar os
  checks dependentes como `not_applicable` enquanto não forem definidos.

`unavailable`, `not_applicable`, `disabled` e `unknown` nunca são sinônimos de
`ok` na UI, no relatório ou no cálculo de saúde.

---

## 2. Requisitos funcionais

| ID | Requisito | Critério de aceite |
| --- | --- | --- |
| SPEC-PROBE-TS-001 | Os checks TS são habilitáveis por perfil versionado e por feed/serviço/PID, sem mudar defaults de reprodução. | Com `transport.enabled = false`, não há alarme nem custo de análise adicional; alterar um limite TOML é refletido em novos eventos com `profile_version`. |
| SPEC-PROBE-TS-002 | `ts` publica observações normalizadas, sem depender de `probe`; a ponte para a Probe usa canal bounded, não bloqueante e contabiliza descartes. | Canal cheio não atrasa demux nem causa panic; descarte torna a janela `incomplete`, não uma conclusão de conformidade. |
| SPEC-PROBE-TS-003 | Detectar e distinguir **TS Sync Loss** de **Sync Byte Error**, registrando offset/bytes descartados e intervalo afetado. | Fixture com byte inválido e fixture com perda de alinhamento abrem os checks corretos, deduplicados por incidente. |
| SPEC-PROBE-TS-004 | Avaliar erro de Continuity Counter por PID, preservando `expected`, `got`, tipo de pacote e quantidade agregada. | Salto de CC em PID de serviço gera um único evento com contagem e contexto do PID; repetição/adaptation-only/null PID não gera falso positivo. |
| SPEC-PROBE-TS-005 | Sinalizar **Transport Error** quando `transport_error_indicator` estiver presente e manter a análise segura. | Pacote TEI abre evento com PID e não derruba demux, Probe ou player. |
| SPEC-PROBE-TS-006 | Tratar CRC inválido, seção truncada/malformada e parsing PSI/SI malformado como evidências distintas, com table id/PID quando disponíveis. | Uma seção de CRC inválido é descartada, contada e persistida; erro de uma seção não invalida as demais. |
| SPEC-PROBE-TS-007 | Avaliar PAT: ausência após janela configurada, tabela inválida e mudanças de versão/mapeamento. | Feed TS estável sem PAT dentro de `pat.max_interval_secs` abre `pat_error`; mudança de programa abre um evento de mudança, não uma falha de CRC. |
| SPEC-PROBE-TS-008 | Avaliar PMT por serviço: ausência, inconsistência com PAT, validade e mudanças de PID PCR/streams/codec. | PAT que anuncia uma PMT ausente no prazo abre evento no serviço; troca de PCR PID ou codec é registrada uma vez por versão. |
| SPEC-PROBE-TS-009 | Avaliar **PID Error** segundo regras declarativas do perfil (PID proibido, obrigatório ausente, associação de serviço inconsistente); PID privado/desconhecido não é erro por si só. | Um PID obrigatório ausente e um PID explicitamente proibido são distinguíveis no event log; PID `private_data` sem regra fica `n/a`. |
| SPEC-PROBE-TS-010 | Medir PCR por PID: repetição, descontinuidade, jitter, frequência/offset e drift quando a amostragem permitir. Separar a precisão de PCR do jitter IP. | Fixture de PCR com salto e outra com jitter produzem métricas e eventos distintos; sem PCR aplicável o estado é `not_applicable`. |
| SPEC-PROBE-TS-011 | Avaliar **PCR Error** e **PCR Accuracy Error** com limites e janela do perfil; registrar valor observado/esperado e a fonte de tempo. | O relatório mostra unidades e limiar; se a fonte de tempo não for confiável, accuracy fica `unavailable`, nunca erro nem verde. |
| SPEC-PROBE-TS-012 | Observar PTS/PES apenas na fronteira TS/PES, detectando ausência, regressão ou intervalo inválido quando o PID é mídia e há dados suficientes. | PID sem PES não é classificado como PTS inválido; regressão em fixture PES abre `pts_error` com PID/serviço e evidência. |
| SPEC-PROBE-TS-013 | Indicar CAT, `scrambling_control`, descritores CA, PIDs privados/dados e legenda DVB quando sinalizados, sem descriptografar ou prometer funcionamento. | CAT ausente só alerta quando o perfil exigir CA; presença de CA/scrambling aparece como metadado, não como prova de falha. |
| SPEC-PROBE-TS-014 | Expor bitrate de multiplex, serviço e PID, taxa de null packets e mudanças relevantes de alocação. | Soma por serviço/PID e total são persistidos a 1 Hz; o cálculo não contém duplicidade de PID compartilhado. |
| SPEC-PROBE-TS-015 | Manter Buffer Analysis, T-STD, MGF e MGB como checks explicitamente opt-in e `not_applicable` até haver perfil fechado e dados suficientes. | Configuração padrão não gera resultado ou severidade para esses checks; ativação sem parâmetros válidos falha na validação de configuração. |
| SPEC-PROBE-TS-016 | Correlacionar eventos TS com perda RTP, descartes locais e indisponibilidade, sem apagar o escopo de serviço/PID nem duplicar a causa raiz. | CC no mesmo tick de `rtp_missing` recebe `caused_by`; sync loss de feed afeta todos os serviços e não abre N alarmes independentes. |
| SPEC-PROBE-TS-017 | Persistir e apresentar estado por transporte/serviço/PID, com valor atual, limite, primeira/última ocorrência, contagem, evidência e referência normativa quando aplicável. | A aba Transport, grade de saúde e exportação mostram `TR 101 290` quando o check tiver mapeamento; filtros por PID/serviço preservam o contexto. |
| SPEC-PROBE-TS-018 | Cobrir regras e adaptação de fronteira com fixtures determinísticas e testes de caracterização, sem panic em dados externos. | `cargo test -p ts` e `cargo test -p probe` exercitam fluxo válido, sync, CC, TEI, CRC, PAT/PMT, PCR, PTS e mudança de tabela. |

---

## 3. Checks e aplicabilidade

| Check operacional | ID | Fonte mínima | Estado quando não mensurável |
| --- | --- | --- | --- |
| TS Sync Loss | `ts_sync_loss` | re-sincronização do demux | `unavailable` se não há bytes TS |
| Sync Byte Error | `sync_byte_error` | pacote/alinhamento inválido | `unavailable` |
| PAT / PMT Error | `pat_error` / `pmt_error` | seções e relógio da Probe | `unknown` antes da janela inicial |
| Continuity Count Error | `cc_error` | cabeçalho TS | `not_applicable` no PID null |
| PID Error | `pid_error` | inventário + perfil | `not_applicable` sem regra |
| Transport Error | `transport_error` | TEI | `unavailable` sem TS |
| CRC Error | `crc_error` | SectionAssembler | `not_applicable` fora de PSI/SI |
| PCR Error / Accuracy | `pcr_error` / `pcr_accuracy_error` | adaptation field + relógio | `not_applicable` / `unavailable` |
| PTS Error | `pts_error` | PES de mídia | `not_applicable` sem PID PES |
| CAT Error | `cat_error` | CAT + política CA | `not_applicable` se CA não é exigida |

As severidades, janelas e limites pertencem ao perfil. Os únicos defaults
seguros são de aplicabilidade e de preservação do comportamento atual; limites
não informados não devem ser inventados a partir da referência visual.

### Rastreabilidade

| Fonte no documento de requisitos | Cobertura |
| --- | --- |
| Print 1 — PCR Analysis, Bitrate, Buffer, T-STD, MGF/MGB | TS-010, TS-011, TS-014, TS-015 |
| Print 2 — 12 Priority Checks | TS-003 a TS-013 |
| Print 3 — tabelas, timing, continuidade, sync, CRC, TEI, mudanças | TS-004 a TS-017 |
| Seção 5.8 — CA, privados, dados e legenda | TS-013 |
| Seções 6–8 — eventos, histórico, UI e recursos | TS-001, TS-002, TS-016 a TS-018 |

