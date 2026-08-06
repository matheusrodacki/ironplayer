# Spec: Probe — Camada 1: IP / UDP / RTP / FEC

- **Spec-IDs:** SPEC-PROBE-IP-001 … SPEC-PROBE-IP-041
- **Crates:** `crates/net` (reformulado) · `crates/probe` · `crates/ui-slint`
- **Fase:** v0.4 — Probe, camada 1
- **Depende de:** [spec-13-probe-mode](../spec-13-probe-mode/spec.md) (sessão, motor de checks, persistência)
- **Referências:** ETSI TR 101 290 §5 · RFC 3550 §A.1 e §6.4.1 · RFC 2733 · SMPTE ST 2022-1/-2 · capturas de tela de probe comercial de referência (06/08/2026)

---

## 1. Objetivo

Responder, para um ponto de rede específico, **antes de olhar para o TS**: os pacotes estão
chegando todos, na ordem, no ritmo certo, e a FEC está coerente? Sem essa camada, todo CC
error do TS fica sem causa atribuída — que é exatamente o problema operacional atual.

---

## 2. Baseline: o que já existe e o que falta

[`crates/net/src/rtp.rs`](../../../crates/net/src/rtp.rs) hoje:

| Já faz                                              | Não faz                                                                            |
| --------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Detecta RTP por V=2 + PT=33 e remove header (12+4·CC)| Não valida PT contra perfil; não lê M, P, X, SSRC, timestamp                        |
| Pass-through quando o 1º byte é `0x47`              | Não valida se o payload é múltiplo de 188 nem quantos TS/datagrama                  |
| Emite `RtpEvent::OutOfOrder { expected, got }`      | Não distingue perda × reordenação × duplicata × pacote antigo; não conta pacotes ausentes |
| Trata wrap 0xFFFF→0x0001                            | Não trata reinício de fonte / troca de SSRC; estado é global, não por SSRC          |
| —                                                   | Não mede tempo de chegada, inter-arrival, PDV ou jitter                             |
| —                                                   | Nenhum suporte a FEC (SMPTE 2022-1)                                                 |

[`crates/net/src/receiver.rs`](../../../crates/net/src/receiver.rs) usa `recv` — **descarta o
endereço de origem**. Trocar por `recv_from` é pré-requisito de vários checks (IP de origem,
múltiplas fontes no mesmo grupo).

---

## 3. Limite fundamental: socket UDP × captura

A probe de referência captura no nível da NIC — a tela de captura expõe o dispositivo
PCI da placa e a MTU do enlace. Um socket UDP **não entrega o cabeçalho IP**. Logo:

| Check do documento base                | Socket UDP (backend padrão) | Captura pcap/Npcap (fase 2) |
| -------------------------------------- | --------------------------- | --------------------------- |
| Perda / reorder / duplicata RTP        | ✅                          | ✅                          |
| Jitter, inter-arrival, PDV             | ✅ (com ressalva §4)        | ✅ (precisão maior)         |
| TS packets por datagrama IP            | ✅                          | ✅                          |
| Tamanho do payload de transporte       | ✅                          | ✅                          |
| Bits RTP (padding, extension, marker)  | ✅                          | ✅                          |
| Payload Type RTP                       | ✅                          | ✅                          |
| FEC L/D, L×D, dois fluxos              | ✅ (join nas portas +2/+4)  | ✅                          |
| **Don't Fragment bit**                 | ❌ não observável           | ✅                          |
| **MTU > 1500 / fragmentação IP**       | ❌ não observável¹          | ✅                          |
| TTL, TOS/DSCP                          | ❌                          | ✅                          |

¹ Um datagrama fragmentado chega remontado pelo kernel; dá para **inferir** MTU excedida
quando o datagrama remontado > 1472 bytes de payload UDP, mas não se distingue de um
emissor que já enviou fragmentado.

| ID                | Requisito                                                                                              | Critério de aceite                                                        |
| ----------------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------- |
| SPEC-PROBE-IP-001 | Backend de aquisição é abstraído (`trait PacketSource`), com implementação `SocketSource` na v1        | Trocar de backend não altera o motor de análise                            |
| SPEC-PROBE-IP-002 | Checks não observáveis pelo backend ativo aparecem como **"não aplicável"**, nunca como verde           | UI mostra `n/a` com tooltip "requer backend de captura"                    |
| SPEC-PROBE-IP-003 | Backend pcap/Npcap fica atrás da feature `capture-pcap`, desligada por padrão                            | `cargo build` sem a feature não exige Npcap instalado                      |

**Recomendação:** não implementar pcap na v1. DF bit e MTU não são o que dói na investigação
atual — perda, reordenação e FEC são.

---

## 4. Precisão da medição em userland (honestidade obrigatória)

A medição de referência mostra inter-arrival médio **701,87 µs com SD de 0,34 µs**.
Esse SD é de captura em hardware. Um socket UDP em Windows, com o timestamp tirado no retorno do `recv`,
tem ruído de **agendamento do SO** na casa de 0,1–2 ms — três ordens de grandeza acima.

Consequências que a spec assume:

- `Instant::now()` no Windows usa QPC (resolução sub-µs) — a resolução do relógio **não** é
  o problema; o agendamento da thread é.
- Medidas absolutas de jitter não são comparáveis com as de uma probe de captura em
  hardware. **Medidas relativas (tendência ao longo de 12 h, picos, rajadas) são.** É isso que a probe entrega.
- Perda e reordenação de RTP são **exatas** — dependem de sequence number, não de tempo.
  São a métrica de confiança primária desta camada.

| ID                | Requisito                                                                                                              | Critério de aceite                                                                         |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| SPEC-PROBE-IP-004 | A thread de recepção roda em prioridade acima do normal e carimba o tempo de chegada imediatamente após o `recv_from`  | Timestamp tirado antes de qualquer parsing ou envio para canal                              |
| SPEC-PROBE-IP-005 | A probe mede seu próprio ruído: SD do inter-arrival nos primeiros `calib_secs` (default 30 s) vira `noise_floor_us`     | `noise_floor_us` gravado no `session.toml` e exibido no painel                              |
| SPEC-PROBE-IP-006 | Alarmes de jitter/inter-arrival só disparam acima de `max(threshold, k × noise_floor_us)` (default `k = 3`)             | Notebook com carga alta não gera falso positivo de jitter                                   |
| SPEC-PROBE-IP-007 | Descartes locais (canal `net_raw` cheio, buffer de socket) são contados por segundo                                     | `local_drops_delta` na linha do CSV                                                         |
| SPEC-PROBE-IP-008 | Evento de perda no mesmo segundo em que houve descarte local é marcado `origin = "local"`, senão `origin = "network"`   | Teste com canal artificialmente saturado produz eventos `origin = local`                    |

---

## 5. Requisitos funcionais

### 5.1 Aquisição

| ID                | Requisito                                                                                              | Critério de aceite                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------- |
| SPEC-PROBE-IP-009 | `recv_from` substitui `recv`; IP:porta de origem é registrado por datagrama                             | Painel mostra `Source IP(s)`; teste de loopback confirma o endereço                 |
| SPEC-PROBE-IP-010 | Detecção de múltiplas fontes no mesmo grupo/porta                                                       | Duas fontes ⇒ evento `multi_source` (Warning) listando ambas                        |
| SPEC-PROBE-IP-011 | Estado da entrada: bytes, datagramas, taxa IP, último pacote, tempo sem dados, erros de socket           | Todos os campos no painel e no CSV                                                  |
| SPEC-PROBE-IP-012 | Ciclo multicast registrado: join, leave, falha de bind, interface indisponível                          | Cada transição vira evento com timestamp                                            |
| SPEC-PROBE-IP-013 | `SO_RCVBUF` efetivo (após truncamento do kernel) é registrado no `session.toml`                          | Valor real, não o solicitado — já há `warn!` no código, falta persistir             |

### 5.2 RTP — parsing e validação

| ID                | Requisito                                                                                                   | Critério de aceite                                                        |
| ----------------- | ----------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| SPEC-PROBE-IP-014 | Parse completo do header: V, P, X, CC, M, PT, seq, timestamp, SSRC (+ header de extensão quando X=1)         | Todos os campos expostos em `RtpHeader`; header de extensão é pulado corretamente |
| SPEC-PROBE-IP-015 | `Invalid Payload Type` — PT fora do configurado (default: aceita 33 e o PT dinâmico da FEC)                  | PT=97 num feed configurado para 33 ⇒ evento com o valor observado         |
| SPEC-PROBE-IP-016 | Bits proibidos por perfil ST 2022-2: Padding, Extension, Marker                                              | Cada um é check independente, habilitável por perfil                      |
| SPEC-PROBE-IP-017 | `Transport packet size` — payload deve ser múltiplo de 188 e começar com `0x47`                              | Payload de 1315 bytes ⇒ evento `bad_payload_size`                         |
| SPEC-PROBE-IP-018 | `TS packets per IP packet` — contado por datagrama; valor fora do perfil (default 7) sinalizado              | Datagrama com 4 TS num perfil de 7 ⇒ evento, sem parar a análise          |

### 5.3 RTP — máquina de sequência por SSRC

Esta é a peça central da camada. Modelo derivado de RFC 3550 §A.1, estendido com janela de
reconciliação para não contar reordenação como perda.

| ID                | Requisito                                                                                                                            | Critério de aceite                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------- |
| SPEC-PROBE-IP-019 | Estado mantido **por SSRC**; troca de SSRC abre evento `ssrc_changed` (Info) e reinicia contadores daquele fluxo                      | Fluxo A→B ⇒ 1 evento, sem explosão de perda falsa                                      |
| SPEC-PROBE-IP-020 | Lacuna de sequência vira **perda provisória**; se o pacote chegar dentro de `reorder_window_ms` (default 200 ms), vira reordenação    | Reorder 100→102→101 ⇒ `out_of_order = 1`, `missing = 0`                                |
| SPEC-PROBE-IP-021 | Perda provisória não reconciliada ao fim da janela vira **perda confirmada**                                                          | Sequência 100→102 sem 101 ⇒ após 200 ms, `missing_confirmed = 1`                       |
| SPEC-PROBE-IP-022 | Duplicata (mesmo seq já recebido) contada e **não reenviada ao analisador TS**                                                        | Duplicata não gera CC error espúrio no TS                                              |
| SPEC-PROBE-IP-023 | `Too old RTP Packet` — seq abaixo da janela de reordenação                                                                            | Chegada de seq 50 quando `max_seq = 300` ⇒ evento `too_old`, não perda negativa        |
| SPEC-PROBE-IP-024 | Reinício de fonte: `MAX_DROPOUT` consecutivo com salto grande reinicia a base sem contabilizar 60 000 perdas                          | Salto de seq 100 → 40 000 ⇒ evento `source_restart`, não `missing = 39 899`            |

```rust
/// SPEC-PROBE-IP-019 … 024 — constantes derivadas de RFC 3550 §A.1
const MAX_DROPOUT:  u16 = 3000;
const MAX_MISORDER: u16 = 100;

pub struct RtpSeqState {
    ssrc: u32,
    base_seq: u32,
    max_seq: u16,
    cycles: u32,          // wraps × 2^16
    received: u64,
    dup: u64,
    out_of_order: u64,
    too_old: u64,
    missing_confirmed: u64,
    /// seq pendente → instante em que a lacuna foi observada
    pending_gaps: BTreeMap<u32, Instant>,
}
// expected = cycles + max_seq - base_seq + 1
// lost     = expected - received   (após reconciliação das lacunas pendentes)
```

**Decisão de projeto — v1 não reordena.** Pacotes seguem para o demux TS na ordem de
chegada. Uma reordenação real produzirá também um CC error no TS; a regra de correlação
(§5.6) marca esse CC error como consequência de IP. Buffer de reordenação fica para a fase 2,
porque adiciona latência e estado, e não muda o diagnóstico — só o mascara.

### 5.4 Temporização

| ID                | Requisito                                                                                                     | Critério de aceite                                                          |
| ----------------- | ------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| SPEC-PROBE-IP-025 | Inter-arrival por datagrama com estatística por janela de 1 s: count, min, max, média, SD (Welford)            | Sem alocação por pacote; SD numericamente estável em 12 h                    |
| SPEC-PROBE-IP-026 | Percentis p50/p95/p99 via histograma de buckets log-espaçados (64 buckets, 10 µs … 100 ms)                     | Memória constante; p99 dentro de ±1 bucket do valor exato em teste sintético |
| SPEC-PROBE-IP-027 | Inter-arrival **esperado** derivado do bitrate observado; desvio relativo exposto como burstiness              | Ver §4.1 — cálculo validado contra a medição de referência                          |
| SPEC-PROBE-IP-028 | Jitter RFC 3550 (`J += (\|D\| − J)/16`) calculado quando o timestamp RTP é utilizável                          | Timestamp não monotônico ou constante ⇒ métrica marcada `n/a`, sem alarme    |
| SPEC-PROBE-IP-029 | Jitter de rede e jitter de PCR são métricas **distintas**, nunca somadas nem exibidas no mesmo eixo            | Painéis e CSV separados                                                      |

#### 4.1 Validação do cálculo contra a medição de referência

```
Bitrate observado    = 15,0024 Mbps
TS por datagrama     = 7  ⇒ payload = 7 × 188 = 1316 bytes
inter-arrival médio  = 1316 × 8 / 15,0024e6 = 701,9 µs
Referência mediu     = 701,87 µs (SD 0,34 µs)   ✓
```

Este cálculo é o teste de sanidade da implementação: com um stream CBR conhecido, a média
medida tem de bater com `payload_bits / bitrate` dentro de 1 %. Vira teste automatizado
(`spec_probe_ip_027_expected_interarrival_matches_cbr`).

Para stream VBR o inter-arrival esperado não é constante — a métrica de burstiness é
marcada `n/a` quando a variação de bitrate na janela excede `vbr_tolerance_pct` (default 5 %).

### 5.5 FEC — SMPTE ST 2022-1

| ID                | Requisito                                                                                                    | Critério de aceite                                                     |
| ----------------- | ------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------- |
| SPEC-PROBE-IP-030 | Descoberta: join opcional em `porta+2` (coluna) e `porta+4` (linha) conforme `fec = auto \| off \| ports:a,b` | `auto` detecta presença sem gerar erro quando não há FEC               |
| SPEC-PROBE-IP-031 | Parse do header FEC de 16 bytes (RFC 2733) após o header RTP                                                 | Campos `SNBase`, `length recovery`, `PT recovery`, `TS recovery`, `D`, `type`, `index`, `offset`, `NA` extraídos |
| SPEC-PROBE-IP-032 | Derivação de L e D: coluna (`D=0`) ⇒ `L = offset`, `D = NA`; linha (`D=1`) ⇒ `offset = 1`, `L = NA`           | Matriz exibida como `L×D` no painel                                    |
| SPEC-PROBE-IP-033 | Check `FEC L range` (default 1..=20) e `FEC D range` (default 4..=20)                                        | Valor fora da faixa ⇒ evento com observado e faixa do perfil           |
| SPEC-PROBE-IP-034 | Check `FEC L×D > 100`                                                                                        | L=20, D=8 ⇒ evento                                                     |
| SPEC-PROBE-IP-035 | Check "dois fluxos FEC quando L ≥ 4" — regra do perfil, marcada como **a validar** contra ST 2022-1           | Só um fluxo com L=8 ⇒ evento Warning, texto indicando origem da regra   |
| SPEC-PROBE-IP-036 | Overhead de FEC medido (bitrate FEC / bitrate principal) e coerência de SSRC/endpoints                        | Painel mostra overhead em %; SSRC do FEC divergente ⇒ evento           |
| SPEC-PROBE-IP-037 | FEC ausente quando o perfil declara FEC obrigatória ⇒ evento; FEC presente sem perfil ⇒ evento Info           | Ambos com debounce ≥ 10 s                                              |
| SPEC-PROBE-IP-038 | Violação de conformidade de encapsulamento **não** interrompe a análise do TS                                | Feed com bit proibido continua sendo demuxado                          |

**v1 não recupera pacotes com FEC** — apenas valida e mede. A estimativa "esta perda teria
sido recuperável pela matriz observada" é a fase 3; é a informação mais valiosa do bloco FEC,
mas exige buffer da matriz completa (L×D pacotes) e não é pré-requisito do diagnóstico.

> **Nota de precisão:** os offsets do header FEC acima vêm do RFC 2733, do qual o
> ST 2022-1 deriva. Os campos e a regra "dois fluxos para L ≥ 4" devem ser conferidos
> contra a norma antes de virarem alarme operacional — até lá, severidade máxima Warning.

### 5.6 Correlação IP → TS

| ID                | Requisito                                                                                                        | Critério de aceite                                                            |
| ----------------- | ---------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| SPEC-PROBE-IP-039 | Perda RTP confirmada é correlacionada com os CC errors ocorridos na mesma janela (default 200 ms)                 | 1 pacote RTP perdido (7 TS) ⇒ CC errors marcados `caused_by = "rtp_missing"`   |
| SPEC-PROBE-IP-040 | O event log não abre alarmes independentes para causa e consequência quando a correlação é conclusiva             | 130 pacotes RTP perdidos ⇒ 1 evento raiz + CC errors agregados como evidência   |
| SPEC-PROBE-IP-041 | CC error **sem** perda RTP correspondente é classificado como originado no TS (upstream do ponto de captura)      | Painel distingue "perda na rede local" de "erro que já chegou no stream"        |

Este é o requisito que dá valor operacional à probe: nas telas de referência aparecem lado a lado
`RTP packet(s) lost: 130` e vários `Continuity Counter Error` — são o mesmo incidente. Sem
correlação, 12 h de log viram ruído.

---

## 6. Persistência da camada IP

Colunas anexadas à linha do `metrics.csv` definido em
[spec-13 §6.1](../spec-13-probe-mode/spec.md#61-metricscsv--colunas-mínimas-da-camada-base):

```
ip_datagrams, ip_bytes, ip_mbps, ts_per_datagram,
rtp_received, rtp_missing_delta, rtp_dup_delta, rtp_reorder_delta, rtp_too_old_delta,
rtp_loss_ratio, ssrc,
iat_min_us, iat_avg_us, iat_max_us, iat_sd_us, iat_p99_us, iat_expected_us,
rfc3550_jitter_us,
fec_present, fec_l, fec_d, fec_overhead_pct,
source_ip, source_count
```

Campos não observáveis ou não aplicáveis são gravados como vazio (`,,`), **nunca** como `0`
— zero significa "medido e deu zero".

---

## 7. Casos de teste obrigatórios

Fixtures geradas por teste, injetando datagramas num socket de loopback (padrão já usado em
[`crates/net/tests/net_loopback.rs`](../../../crates/net/tests/net_loopback.rs)).

| Cenário                                                     | Comportamento esperado                                              |
| ----------------------------------------------------------- | -------------------------------------------------------------------- |
| Sequência contínua 1000 pacotes                             | `missing = 0`, `dup = 0`, `out_of_order = 0`                         |
| Seq 100 → 102 (sem 101), nada mais chega                    | após `reorder_window_ms`: `missing_confirmed = 1`                    |
| Seq 100 → 102 → 101 dentro de 50 ms                         | `out_of_order = 1`, `missing_confirmed = 0`                          |
| Seq 100 → 102 → 101 após 500 ms                             | `missing_confirmed = 1`, `too_old = 1`                               |
| Seq 100 enviado duas vezes                                  | `dup = 1`; payload entregue ao demux **uma** vez                     |
| Wrap 0xFFFF → 0x0000                                        | `cycles += 1`, sem perda contabilizada                               |
| Salto seq 100 → 40 000                                      | `source_restart`, sem perda em massa                                 |
| Troca de SSRC                                               | `ssrc_changed`, contadores reiniciados para o novo SSRC              |
| PT = 97 com perfil PT = 33                                  | evento `invalid_payload_type` com valor observado                    |
| Payload de 1315 bytes                                       | evento `bad_payload_size`; datagrama não é passado ao demux          |
| Datagrama com 4 TS (perfil 7)                               | evento `ts_per_datagram`; análise continua                           |
| X=1 com header de extensão de 8 bytes                       | payload correto após pular a extensão                                |
| CBR 15 Mbps, 7 TS/datagrama                                 | `iat_avg_us` = 702 ± 1 %                                             |
| Canal `net_raw` saturado artificialmente                    | perda marcada `origin = local`                                       |
| FEC coluna com offset=8, NA=5                               | `L = 8`, `D = 5`, `L×D = 40` ⇒ sem alarme                            |
| FEC com offset=20, NA=8                                     | evento `fec_lxd_gt_100`                                              |
| Feed sem FEC com `fec = auto`                               | `fec_present = false`, nenhum alarme                                 |
| 1 pacote RTP perdido + CC errors em 100 ms                  | CC errors marcados `caused_by = rtp_missing`                         |

Nomes seguem a convenção do projeto: `spec_probe_ip_020_reorder_within_window`, etc.

---

## 8. Limiares default propostos

Derivados dos números reais da medição de referência (3 h 34 min de monitoração,
18 342 311 pacotes capturados, 1036 ausentes ⇒ razão de perda 5,6 × 10⁻⁵), não de
zero-tolerância — um multicast de produção real não tem perda zero.

```toml
[probe.checks.rtp_missing]
threshold      = 0        # qualquer perda confirmada conta
severity       = "error"
min_duration   = "0s"     # perda é pontual: abre imediatamente
summary_window = "60s"    # mas agrega por minuto

[probe.checks.rtp_loss_ratio]
threshold      = 1e-4     # ~2× o observado na referência; acima disso é degradação
window         = "300s"
severity       = "error"
min_duration   = "60s"

[probe.checks.rtp_reorder]
threshold      = 0
severity       = "warning"
summary_window = "60s"

[probe.checks.rtp_duplicate]
threshold      = 0
severity       = "warning"

[probe.checks.iat_max]
threshold_us   = 5000     # 50 ms na referência é "Major"; 5 ms já é rajada num CBR de 700 µs
noise_k        = 3.0      # SPEC-PROBE-IP-006
severity       = "warning"
min_duration   = "10s"

[probe.checks.ts_per_datagram]
expected       = 7
severity       = "warning"

[probe.checks.fec]
mode           = "auto"   # auto | off | ports:2,4
l_range        = [1, 20]
d_range        = [4, 20]
max_lxd        = 100
severity       = "warning"

reorder_window_ms = 200
calib_secs        = 30
vbr_tolerance_pct = 5
```

Todos os valores são **propostas para calibrar na primeira sessão de 12 h**, não normativos.
O relatório de sessão deve listar quantas vezes cada check disparou, justamente para o
ajuste dos limiares na segunda rodada.

---

## 9. Faseamento

| Fase | Entrega                                                                                       | Justificativa                                            |
| ---- | --------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| 1    | `recv_from`, parse RTP completo, máquina de sequência por SSRC, inter-arrival/PDV, correlação IP→TS | É o diagnóstico que falta hoje e não depende de nada externo |
| 2    | FEC 2022-1: descoberta, parse, L/D, overhead                                                  | Alto valor, mas exige joins extras e validação normativa  |
| 3    | Backend pcap (DF bit, MTU, TTL) e estimativa de recuperação por FEC                            | Só se a investigação exigir camada IP abaixo do UDP       |

---

## 10. Questões em aberto

| # | Questão                                                                                                     | Efeito se a resposta mudar                              |
| - | ----------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| 1 | Os feeds investigados usam RTP ou UDP puro? Em UDP puro, toda a §5.2/5.3 fica `n/a` e sobra só CC do TS       | Define se a camada IP entrega diagnóstico ou não        |
| 2 | Há FEC configurada nos feeds da casa? Em quais portas?                                                       | Decide se a fase 2 vale a pena                          |
| 3 | O perfil de encapsulamento é ST 2022-2 declarado, ou apenas "RTP com 7 TS"?                                  | Define quais checks de bits proibidos ficam ativos      |
| 4 | Vale reordenar pacotes antes do demux (mascara o problema, melhora o thumbnail) ou manter só contagem?        | v1 assume só contagem                                   |
| 5 | O notebook fica na mesma VLAN/porta espelhada do ponto sob investigação, ou atrás de switch com IGMP snooping?| Perda medida pode ser do caminho até o notebook, não do feed |
