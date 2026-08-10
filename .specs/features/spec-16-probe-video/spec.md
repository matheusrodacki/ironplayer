# Spec: Probe — checks de vídeo

- **Spec-IDs:** SPEC-PROBE-VID-001 a SPEC-PROBE-VID-016
- **Crates:** `crates/av` · `crates/ts` · `crates/probe` · `crates/ui-slint` · composição em `src/`
- **Fase:** v0.4 Probe, após a futura `spec-15-probe-ts`
- **Origem:** `.notVersioned/Requisitos_Probe_MPEGTS_v0.4.docx`, §§ 1.3, 5.2–5.5, 6 e 7

---

## 1. Objetivo e limites

Transformar os dados de vídeo já identificados pelo IronPlayer em checks versionados,
por serviço e PID, sem confundir ausência de sinal, falha do decoder de reprodução e
qualidade perceptual. A feature reutiliza PES, Media Info e o decoder FFmpeg, mas não
usa o FFmpeg como parser primário de MPEG-TS.

`video_missing` já existente é somente presença/bitrate de PID vindo da PMT. Esta spec
não o substitui: seus checks começam apenas quando existe um componente de vídeo
aplicável e declaram `n/a` enquanto não houver dados suficientes, codec suportado ou
detector habilitado.

Não há valores operacionais aprovados para as métricas perceptuais. Os defaults de
perfil desta feature devem ser conservadores e desabilitados quando dependerem de uma
política ainda não decidida (blockiness, distorção localizada, logo e requisitos HDR).

## 2. Requisitos funcionais

| ID | Requisito | Critério de aceite |
| --- | --- | --- |
| SPEC-PROBE-VID-001 | Habilitar análise por serviço/PID e por perfil, com orçamento explícito de CPU/memória e amostragem configurável. | Desabilitar um serviço não instancia decoder/analisador para ele; saturação reduz amostragem antes de afetar reprodução. |
| SPEC-PROBE-VID-002 | Publicar snapshot de metadados: resolução, aspecto, codec/perfil/nível, frame rate, scan type e Active Format quando presente. | UI e relatório mostram origem sinalizada/observada; mudança gera evento deduplicado com valor anterior/novo. |
| SPEC-PROBE-VID-003 | Extrair e validar metadados HDR: contraste quando disponível, MaxCLL, MaxFALL e mastering display (primárias, ponto branco, luminâncias). | Stream sem HDR fica `n/a`; ausência só alerta se o perfil exigir o metadado; valores inválidos carregam campo e motivo. |
| SPEC-PROBE-VID-004 | Separar estado de entrada, PES, análise e decoder. | PES indisponível, decoder indisponível e quadro não recebido são estados distintos; nenhum deles abre freeze/black por inferência. |
| SPEC-PROBE-VID-005 | Entregar observações de quadros decodificados por canal bounded, com PTS, dimensões, formato e luminância reduzida. | Um consumidor lento descarta observação secundária contabilizada; a thread de decode/render nunca bloqueia. |
| SPEC-PROBE-VID-006 | Detectar Freeze Frames por repetição ou falta de evolução perceptível durante janela e duração do perfil. | Evento registra início, fim, duração, serviço, PID, score e limites; movimento normal e falta de decoder não geram falso positivo. |
| SPEC-PROBE-VID-007 | Detectar Black Frames por limiar de luminância, percentual de área e duração configuráveis. | Conteúdo preto curto abaixo da duração não alarma; evento traz luma, cobertura e duração medidas. |
| SPEC-PROBE-VID-008 | Medir blockiness nas fronteiras de blocos/CTUs e sinalizar score acima do perfil. | Score atual, máximo da janela e limite aparecem no evento/relatório; codecs ou formatos sem método suportado ficam `n/a`. |
| SPEC-PROBE-VID-009 | Detectar blocky distortions persistentes ou localizadas, distinguindo ocorrência transitória quando possível. | Evento informa score, área/região quando disponível, contagem e persistência; limiares/método são versionados no perfil. |
| SPEC-PROBE-VID-010 | Medir GOP length, distribuição, mínimo/máximo e mudanças por codec/serviço. | GOPs válidos produzem série e mudança fora do perfil gera evento com GOP anterior/atual. |
| SPEC-PROBE-VID-011 | Registrar erros de elementary stream para MPEG-2 Video, H.264 e HEVC: headers ausentes, sintaxe/bytes inválidos, tamanho de quadro, referências e picture data incompleta quando aplicável. | Erro inclui codec, PID, serviço, PTS/frame/GOP/offset quando disponíveis e não derruba pipeline. |
| SPEC-PROBE-VID-012 | Correlacionar erros de vídeo com CC, PES, PTS/DTS e PCR sem multiplicar a mesma causa raiz em alarmes independentes. | Evento filho referencia a evidência TS; relatório agrupa causa provável preservando todos os contextos. |
| SPEC-PROBE-VID-013 | Detectar logo conforme referência, região, tolerância e duração configuradas por serviço. | Referência ausente/inválida deixa check desabilitado ou `n/a`; presença, ausência e alteração são eventos distintos. |
| SPEC-PROBE-VID-014 | Persistir métricas, transições e evidências no formato da sessão Probe e exibi-las nas abas Vídeo/Eventos. | Cada check mostra estado, valor, unidade, limite, contagem, primeira/última ocorrência e `profile_version`. |
| SPEC-PROBE-VID-015 | Aplicar debounce, histerese, deduplicação e aplicabilidade a todos os checks. | Condição contínua gera um evento agregado; o fechamento só ocorre após a histerese configurada. |
| SPEC-PROBE-VID-016 | Aceitar fixtures e injeção determinística de PES, metadados e frames reduzidos. | Testes cobrem codec suportado/não suportado, ausência de decoder, freeze, black, mudança de metadado, erro e backpressure sem depender de GPU/tela. |

## 3. Rastreabilidade

| Requisito de origem | Cobertura nesta spec |
| --- | --- |
| RF-VID-Q-001…004 | SPEC-PROBE-VID-006…009 |
| RF-VID-M-001…006 | SPEC-PROBE-VID-002 |
| RF-VID-M-007 | SPEC-PROBE-VID-013 |
| RF-VID-HDR-001…007 | SPEC-PROBE-VID-003 |
| RF-VID-MEA-001 | SPEC-PROBE-VID-010 |
| RF-VID-DEC-001…011 | SPEC-PROBE-VID-011 e SPEC-PROBE-VID-012 |
| Requisitos transversais de §§ 6–8 | SPEC-PROBE-VID-001, 004, 005, 014…016 |

## 4. Não-objetivos e decisões pendentes

- Não fazer análise perceptual na cadeia de renderização nem abrir dispositivo de áudio.
- Não classificar falha isolada do decoder de reprodução como erro do bitstream.
- Não fixar, nesta versão, limiares de freeze, black, blockiness, distorção, logo ou
  política de HDR: todos pertencem a perfil auditável.
- Não incluir tone mapping, QoE composto, T-STD, CA/descriptografia ou análise editorial.
