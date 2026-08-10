# Spec: Probe — checks de áudio

- **Spec-IDs:** SPEC-PROBE-AUD-001 a SPEC-PROBE-AUD-014
- **Crates:** `crates/av` · `crates/ts` · `crates/probe` · `crates/ui-slint` · composição em `src/`
- **Fase:** v0.4 Probe, após a futura `spec-15-probe-ts`
- **Origem:** `.notVersioned/Requisitos_Probe_MPEGTS_v0.4.docx`, §§ 1.3, 5.6–5.7, 6 e 7

---

## 1. Objetivo e limites

Adicionar análise de áudio por serviço, PID, trilha e canal ao modo Probe. Ela usa PCM
decodificado exclusivamente para medição, sem abrir saída WASAPI/`cpal` no modo Probe e
sem alterar sincronização ou volume do player.

`audio_missing` existente permanece um check de presença/bitrate de PID da PMT. Silêncio
é uma conclusão sobre PCM válido e disponível; PES/decoder ausente, drop de observação e
trilha não aplicável são estados próprios, nunca sinônimos de silêncio.

Os modos normativos de loudness e os limiares operacionais ainda dependem de decisão do
produto. Portanto o perfil padrão usa `loudness.mode = "none"`, que torna a métrica `n/a`
até habilitação explícita e auditável.

## 2. Requisitos funcionais

| ID | Requisito | Critério de aceite |
| --- | --- | --- |
| SPEC-PROBE-AUD-001 | Habilitar análise por serviço/PID/trilha e limitar custo por perfil. | Desabilitar trilha não decodifica PCM para análise; sobrecarga reduz análise secundária antes do player. |
| SPEC-PROBE-AUD-002 | Publicar idioma, sampling frequency, quantidade/layout de canais e codec/perfil/parâmetros relevantes. | Snapshot e relatório mostram valores por trilha; ausência/mudança/inconsistência geram evento com anterior/novo. |
| SPEC-PROBE-AUD-003 | Separar presença de PID, disponibilidade de PES, disponibilidade do decoder e silêncio de conteúdo. | Trilha sem PCM válido fica indisponível/`n/a`; não abre `audio_silence`. |
| SPEC-PROBE-AUD-004 | Entregar blocos PCM normalizados por canal em canal bounded, com tempo monotônico e PTS quando disponível. | Consumidor lento não bloqueia decode; drop é contabilizado e invalida somente a janela afetada. |
| SPEC-PROBE-AUD-005 | Medir loudness atual, integrado e de janela no modo configurado, com unidade, limite e versão do perfil. | `mode = none` mostra `n/a`; modo habilitado identifica algoritmo/unidade e produz valores reprodutíveis. |
| SPEC-PROBE-AUD-006 | Detectar silêncio por canal/trilha abaixo de limiar e duração configuráveis. | PCM abaixo do limiar abre após duração; PES/decoder ausente, lacuna de observação e histerese são tratados corretamente. |
| SPEC-PROBE-AUD-007 | Medir nível por canal e sinalizar faixa operacional excedida. | Evento traz canal, nível atual/máximo, faixa e duração; níveis normais fecham após histerese. |
| SPEC-PROBE-AUD-008 | Detectar ausência/inatividade de par estéreo ou canal esperado. | Perfil declara pares esperados; mono não vira erro de estéreo sem configuração explícita. |
| SPEC-PROBE-AUD-009 | Medir true peak por canal com método e oversampling declarados pelo perfil. | Ultrapassagem informa dBTP, canal, limite, quantidade e duração. |
| SPEC-PROBE-AUD-010 | Detectar jitter noise/descontinuidade perceptível pelo método suportado e perfil. | Método sem suporte fica `n/a`; evento inclui indicador, janela e evidência temporal. |
| SPEC-PROBE-AUD-011 | Detectar clipping por amostras/intervalos e registrar canais, quantidade e duração. | Clipping transitório abaixo da duração não alarma; evento agregado não é emitido por amostra. |
| SPEC-PROBE-AUD-012 | Persistir métricas, transições e contexto de áudio na sessão e apresentar abas Áudio/Eventos. | Estado, valor, limite, canal, contagem, timestamps e `profile_version` aparecem no relatório/UI. |
| SPEC-PROBE-AUD-013 | Aplicar debounce, histerese, deduplicação e `n/a`/desabilitado/indisponível de forma uniforme. | Condição contínua produz evento agregado e só fecha após a janela configurada. |
| SPEC-PROBE-AUD-014 | Aceitar PCM e metadados injetados em fixtures determinísticas. | Testes cobrem mono/estéreo, silêncio real, ausência de decoder, clipping, mudança de formato e backpressure sem device de áudio. |

## 3. Rastreabilidade

| Requisito de origem | Cobertura nesta spec |
| --- | --- |
| RF-AUD-L-001 | SPEC-PROBE-AUD-005 |
| RF-AUD-Q-001…006 | SPEC-PROBE-AUD-006…011 |
| RF-AUD-M-001…004 | SPEC-PROBE-AUD-002 |
| Requisitos transversais de §§ 6–8 | SPEC-PROBE-AUD-001, 003, 004, 012…014 |

## 4. Não-objetivos e decisões pendentes

- Não abrir device de áudio nem aplicar volume/mute/normalização no áudio do player.
- Não chamar ausência de PES/decoder de silêncio ou defeito de conteúdo.
- Não escolher implicitamente EBU R 128, ATSC A/85 ou outro padrão: o modo, unidade e
  gating de loudness são decisão de perfil/produto.
- Não prometer medição de jitter noise para codecs/métodos sem evidência PCM confiável.
