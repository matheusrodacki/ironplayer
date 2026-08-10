# Design: Probe — checks e medições MPEG-TS

## 1. Arquitetura e limites de dependência

`ts` continua dono do parsing e emite fatos de transporte. `probe` traduz
fatos em medições versionadas, aplica perfil, deduplica e persiste. A UI só lê
snapshots imutáveis; ela nunca faz parsing nem aguarda o pipeline.

```text
UDP/RTP ──> ts::TsDemuxer ──> TsEvent / PcrEvent / PSI snapshot
                      │                         │
                      └──> MetricsSnapshot      └──> ProbeTsAdapter
                                                       │ bounded try_send
                                                       v
                                             probe::ProbeEngine (1 Hz)
                                                       │
                                      events.jsonl / series / watch snapshot
                                                       v
                                                     UI/relatório
```

`crates/ts` não importa `probe`. A composição em `src`/`feed` lê os eventos,
agrega fatos de alta frequência entre dois ticks e passa uma observação compacta
para o `ProbeEngine`. Não é permitido serializar um pacote TS completo no canal
de análise, nem bloquear o demux para entregar um evento.

## 2. Contrato de observação

Os tipos abaixo são a API-alvo; cada função pública que os expuser deve portar
o doc-comment do respectivo SPEC-ID.

```rust
/// SPEC-PROBE-TS-002
#[derive(Debug, Clone, Default)]
pub struct TsProbeTick {
    pub interval_incomplete: bool,
    pub sync_loss: u64,
    pub sync_byte_errors: Vec<SyncByteEvidence>,
    pub tei_by_pid: BTreeMap<Pid, u64>,
    pub cc_by_pid: BTreeMap<Pid, CcEvidence>,
    pub crc_by_pid_table: BTreeMap<(Pid, u8), u64>,
    pub psi: PsiAvailability,
    pub pcr_by_pid: BTreeMap<Pid, PcrObservation>,
    pub pes_by_pid: BTreeMap<Pid, PesTimingObservation>,
}

/// SPEC-PROBE-TS-010
#[derive(Debug, Clone, Default)]
pub struct PcrObservation {
    pub samples: u64,
    pub discontinuities: u64,
    pub jitter_max_us: Option<i64>,
    pub repeat_interval_us: Option<i64>,
    pub frequency_offset_ppm: Option<f64>,
    pub drift_ppm_per_s: Option<f64>,
    pub source_clock: ClockQuality,
}
```

`TsProbeTick` contém contadores/deltas de um intervalo, não totais. Campos
opcionais expressam ausência de evidência. Dados externos, including section
malformada, só chegam aqui após `Result`; não há `unwrap`, indexação não
verificada ou conversão truncante.

O adaptador possui uma fila bounded. Em `try_send` cheio, incrementa
`probe_ts_observation_drops`, marca o próximo tick como `interval_incomplete`
e emite a saúde local já prevista pela spec-13. Uma janela incompleta pode
incrementar uma evidência recebida, mas não pode concluir ausência de PAT/PMT,
PID, PCR ou PTS.

## 3. Fatos, checks e estado

### 3.1 Fatos novos em `ts`

Os eventos existentes são preservados. A expansão proposta é aditiva:

- `TsEvent::SyncByteError { offset, got }`, separado de `SyncLost`;
- `TsEvent::TransportError { pid }` para TEI;
- `TsEvent::PsiMalformed { pid, table_id: Option<u8>, kind }`;
- snapshots de PAT/PMT/CAT com versão, instante monotônico e hash estrutural;
- observação PES com PTS/DTS opcional e motivo de pacote incompleto.

O parser não deve inferir erro de PID porque um PID não aparece numa PMT: o
inventário pertence à camada de Probe e o perfil decide o que é obrigatório ou
proibido. PIDs privados, `0x1FFF` e compartilhamento de infraestrutura recebem
tratamento de aplicabilidade explícito.

### 3.2 Chaves, agregação e correlação

O estado do motor é indexado por:

```text
(feed_id, service_id?, pid?, check_id)
```

`service_id` vem do inventário PAT/PMT da spec-13. Um PID só ganha o primeiro
dono que o inventário declarar; eventos sem PID (sync loss) permanecem no
transporte e são projetados para todos os serviços na visualização, não
duplicados no armazenamento.

`CcEvidence` preserva `expected`, `got` e a primeira/última ocorrência.
Para PSI, a chave inclui `table_id`, mas o evento operacional pode continuar
agregado como `crc_error` por PID. Mudanças de tabela são comparadas por
versão+hash estrutural: mesma tabela repetida não abre novo evento.

Uma correlação acrescenta `caused_by` sem remover PID ou serviço. Exemplo:
perda RTP confirmada no tick torna o CC `caused_by = rtp_missing`; uma queda de
canal local usa `origin = local`; os dois fatos continuam visíveis.

### 3.3 PAT, PMT, CAT e temporizadores

`PsiAvailability` registra primeiro/último instante válido por tabela e por
serviço. A avaliação só começa depois de uma grace window configurável e é
suspensa em feed indisponível ou intervalo incompleto. PAT/PMT inválida é
separada de PAT/PMT ausente; uma alteração válida de versão é informativa.

CAT só é um erro caso `ca_required = true` no perfil. `scrambling_control` e
descritores CA são inventário, sem tentativa de descriptografia. Legenda DVB e
dados privados são classificados quando o descriptor permitir, senão
`unknown`, não `ok`.

### 3.4 PCR e PTS

O relógio de chegada é monotônico. Jitter PCR compara deltas PCR e de chegada;
ele não é jitter IP. `frequency_offset_ppm` e `drift_ppm_per_s` exigem número
mínimo de amostras e clock com qualidade declarada. Sem uma fonte confiável,
`pcr_accuracy_error` é `unavailable`.

PTS só é analisado após identificação de PID PES de mídia e uma sequência
completa. Falta de PES é disponibilidade da camada seguinte, não PTS inválido.
O tipo de observação deixa espaço para `Dts` e `PesGap`, mas esta feature não
classifica falha de decoder como erro do TS.

## 4. Perfil de configuração

Os defaults moram em `[probe.checks]`, reaproveitando `CheckDef` da spec-13.
O bloco específico não pode trocar os defaults de player:

```toml
[probe.transport]
enabled = true
pat_max_interval_secs = 0.5
pmt_max_interval_secs = 0.5
psi_grace_secs = 2.0
ca_required = false

[probe.transport.pcr]
min_samples = 8
accuracy_enabled = false # requer clock/profile definido

[probe.transport.tstd]
enabled = false # exige modelo e parâmetros obrigatórios
```

Cada check pode ser disabled. Campos finitos, não negativos e dependências de
perfil são validados ao carregar configuração. Ativar accuracy/T-STD/MGF/MGB
sem parâmetros suficientes retorna erro de configuração; não abre alarmes com
defaults ocultos.

## 5. UI, persistência e degradação

A aba **Transport** organiza: Overview, PCR, PSI/Tables e CA/Data. Cada linha
mostra estado, medição, unidade, limite, contagem e evidência curta. O event
log usa descrições como `TR 101 290 P1.4 Continuity Counter Error`, quando o
mapeamento existir; a referência não transforma automaticamente um valor em
limite operacional.

Eventos e amostras continuam sendo persistidos pela sessão da spec-13. O
evento deve carregar `check_id`, `profile_version`, `service_id`, `pid`,
`first_seen`, `last_seen`, `count`, valor observado/esperado e `evidence`.

Sob sobrecarga, a ordem é: preservar demux e contadores; coalescer evidências
por chave; suspender séries PCR secundárias; registrar `probe_degraded`. Não
se descartam eventos críticos silenciosamente, e a UI recebe snapshots por
`watch` a 1 Hz.

