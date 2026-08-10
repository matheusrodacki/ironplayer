//! Amostra de 1 Hz e derivação de deltas a partir dos contadores acumulados.
//!
//! Decisão de projeto do §5.3: o `ProbeEngine` **amostra contadores
//! acumulados** em vez de fazer tee dos eventos brutos.  `ErrorSnapshot` já
//! expõe `cc_errors` por PID, `crc_errors` por `(pid, table_id)`,
//! `sync_losses`, `rtp_out_of_order` e `udp_overflows` de forma cumulativa —
//! a probe deriva o delta por segundo.  Isso evita duplicar canais e mantém a
//! regra de "não duplicar métrica existente".
//!
//! SPEC-PROBE-005 · SPEC-PROBE-006 · §6.1

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ts::metrics::{MetricsSnapshot, PidType};
use ts::Pid;

use crate::ip::{csv_opt, csv_opt_f64, IpTick};
use crate::session::Encapsulation;
use crate::severity::Severity;

/// Versão do layout de colunas de `metrics.csv`.
///
/// Versão 2 acrescenta as colunas da camada IP (spec-14 §6).  As colunas da
/// camada base **não** mudam de posição nem de nome, então uma planilha da
/// versão 1 continua legível; a versão existe para que quem lê o CSV saiba se
/// pode esperar as colunas `rtp_*`/`fec_*`.
///
/// §6.1 · spec-14 §6
pub const CSV_SCHEMA_VERSION: u32 = 2;

/// Cabeçalho de `metrics.csv` — a ordem das colunas é fixa e versionada.
///
/// §6.1 · spec-14 §6
pub const CSV_HEADER: &str = "ts_utc,uptime_s,connected,bitrate_kbps,null_ratio,\
cc_errors_delta,crc_errors_delta,sync_loss_delta,pcr_jitter_delta,pcr_disc_delta,\
local_drops_delta,sched_jitter_ms,worst_severity,\
encapsulation,ip_datagrams,ip_bytes,ip_mbps,ts_per_datagram,\
rtp_received,rtp_missing_delta,rtp_dup_delta,rtp_reorder_delta,rtp_too_old_delta,\
rtp_loss_ratio,ssrc,\
iat_min_us,iat_avg_us,iat_max_us,iat_sd_us,iat_p99_us,iat_expected_us,\
rfc3550_jitter_us,\
fec_present,fec_l,fec_d,fec_overhead_pct,\
source_ip,source_count";

/// Contadores brutos que a probe amostra a cada tick.
///
/// Separado de [`ProbeSample`] porque é o que a [`CounterBaseline`] guarda
/// entre ticks: os cumulativos, não os deltas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawCounters {
    pub cc_errors_by_pid: HashMap<Pid, u64>,
    /// CRC inválidos por PID de tabela — `ErrorSnapshot` guarda por
    /// `(pid, table_id)`; aqui as tabelas do mesmo PID são somadas, porque o
    /// escopo da grade é o PID (SPEC-PROBE-023).
    pub crc_by_pid: HashMap<Pid, u64>,
    pub crc_errors: u64,
    pub sync_losses: u64,
    /// SPEC-PROBE-TS-003
    pub sync_byte_errors: u64,
    /// SPEC-PROBE-TS-005
    pub transport_errors_by_pid: HashMap<Pid, u64>,
    /// SPEC-PROBE-TS-006
    pub psi_malformed_by_pid: HashMap<Pid, u64>,
    /// SPEC-PROBE-TS-012
    pub pts_errors_by_pid: HashMap<Pid, u64>,
    pub pcr_jitter_by_pid: HashMap<Pid, u64>,
    pub pcr_jitter_events: u64,
    pub pcr_disc_by_pid: HashMap<Pid, u64>,
    pub pcr_discontinuities: u64,
    pub rtp_out_of_order: u64,
    pub udp_overflows: u64,
}

impl RawCounters {
    /// Extrai os cumulativos de um `MetricsSnapshot`.
    ///
    /// §5.3
    pub fn from_metrics(m: &MetricsSnapshot) -> Self {
        let mut crc_by_pid: HashMap<Pid, u64> = HashMap::new();
        for ((pid, _table_id), count) in &m.errors.crc_errors {
            *crc_by_pid.entry(*pid).or_insert(0) += *count;
        }

        // Os logs de PCR são vetores de eventos, não contadores: contar por PID
        // aqui é o que permite atribuir jitter/descontinuidade à linha do PID na
        // grade.  Ambos são limitados por `max_error_log_entries` no crate `ts`,
        // então saturam numa sessão longa — a mesma limitação que o total já
        // tinha, agora só visível por PID.
        let mut pcr_jitter_by_pid: HashMap<Pid, u64> = HashMap::new();
        for record in &m.errors.pcr_jitter_events {
            *pcr_jitter_by_pid.entry(record.pid).or_insert(0) += 1;
        }
        let mut pcr_disc_by_pid: HashMap<Pid, u64> = HashMap::new();
        for record in &m.errors.pcr_discontinuities {
            *pcr_disc_by_pid.entry(record.pid).or_insert(0) += 1;
        }

        Self {
            cc_errors_by_pid: m.errors.cc_errors.clone(),
            crc_errors: crc_by_pid.values().sum(),
            crc_by_pid,
            sync_losses: m.errors.sync_losses,
            sync_byte_errors: m.errors.sync_byte_errors,
            transport_errors_by_pid: m.errors.transport_errors.clone(),
            psi_malformed_by_pid: m.errors.psi_malformed.clone(),
            pts_errors_by_pid: m.errors.pts_errors.clone(),
            pcr_jitter_events: pcr_jitter_by_pid.values().sum(),
            pcr_jitter_by_pid,
            pcr_discontinuities: pcr_disc_by_pid.values().sum(),
            pcr_disc_by_pid,
            rtp_out_of_order: m.errors.rtp_out_of_order,
            udp_overflows: m.errors.udp_overflows,
        }
    }
}

/// Deltas de um tick, já classificados por PID onde faz sentido.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CounterDeltas {
    /// CC errors novos por PID — vira contexto do evento (SPEC-PROBE-008).
    pub cc_by_pid: HashMap<Pid, u64>,
    pub cc_total: u64,
    /// CRC inválidos novos por PID de tabela (SPEC-PROBE-021).
    pub crc_by_pid: HashMap<Pid, u64>,
    pub crc: u64,
    pub sync_loss: u64,
    /// SPEC-PROBE-TS-003
    pub sync_byte_errors: u64,
    /// SPEC-PROBE-TS-005
    pub transport_errors_by_pid: HashMap<Pid, u64>,
    /// SPEC-PROBE-TS-006
    pub psi_malformed_by_pid: HashMap<Pid, u64>,
    /// SPEC-PROBE-TS-012
    pub pts_errors_by_pid: HashMap<Pid, u64>,
    /// Jitter de PCR novo por PID (SPEC-PROBE-021).
    pub pcr_jitter_by_pid: HashMap<Pid, u64>,
    pub pcr_jitter: u64,
    /// Descontinuidade de PCR nova por PID (SPEC-PROBE-021).
    pub pcr_disc_by_pid: HashMap<Pid, u64>,
    pub pcr_disc: u64,
    pub rtp_out_of_order: u64,
    pub udp_overflows: u64,
}

/// Estado entre ticks para transformar cumulativos em deltas.
///
/// Contadores que **diminuem** (reset de sessão, `ResetErrors` da UI,
/// reconexão) são tratados como reinício: o delta é o novo valor, nunca
/// negativo.  Sem isso, um `ResetErrors` produziria deltas absurdos ou
/// underflow.
///
/// §5.3 · RNF-PRB-003
#[derive(Debug, Clone, Default)]
pub struct CounterBaseline {
    prev: RawCounters,
    primed: bool,
}

impl CounterBaseline {
    /// Cria uma baseline não inicializada.
    pub fn new() -> Self {
        Self::default()
    }

    /// Descarta a baseline — o próximo tick vira o novo ponto zero.
    ///
    /// Usado ao reconectar (SPEC-PROBE-011): os contadores do pipeline são
    /// zerados junto com o `Reset`, e a probe não deve reportar isso como
    /// rajada de erros.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Consome os cumulativos deste tick e devolve os deltas.
    ///
    /// O **primeiro** tick após um reset devolve deltas zerados: sem ponto de
    /// comparação, qualquer valor seria uma leitura inventada.
    ///
    /// §5.3
    pub fn delta(&mut self, now: RawCounters) -> CounterDeltas {
        if !self.primed {
            self.prev = now;
            self.primed = true;
            return CounterDeltas::default();
        }

        let cc_by_pid = delta_by_pid(&now.cc_errors_by_pid, &self.prev.cc_errors_by_pid);
        let crc_by_pid = delta_by_pid(&now.crc_by_pid, &self.prev.crc_by_pid);
        let transport_errors_by_pid = delta_by_pid(
            &now.transport_errors_by_pid,
            &self.prev.transport_errors_by_pid,
        );
        let psi_malformed_by_pid =
            delta_by_pid(&now.psi_malformed_by_pid, &self.prev.psi_malformed_by_pid);
        let pts_errors_by_pid = delta_by_pid(&now.pts_errors_by_pid, &self.prev.pts_errors_by_pid);
        let pcr_jitter_by_pid = delta_by_pid(&now.pcr_jitter_by_pid, &self.prev.pcr_jitter_by_pid);
        let pcr_disc_by_pid = delta_by_pid(&now.pcr_disc_by_pid, &self.prev.pcr_disc_by_pid);

        let deltas = CounterDeltas {
            cc_total: cc_by_pid.values().sum(),
            cc_by_pid,
            crc: crc_by_pid.values().sum(),
            crc_by_pid,
            sync_loss: now.sync_losses.saturating_sub(self.prev.sync_losses),
            sync_byte_errors: now
                .sync_byte_errors
                .saturating_sub(self.prev.sync_byte_errors),
            transport_errors_by_pid,
            psi_malformed_by_pid,
            pts_errors_by_pid,
            pcr_jitter: pcr_jitter_by_pid.values().sum(),
            pcr_jitter_by_pid,
            pcr_disc: pcr_disc_by_pid.values().sum(),
            pcr_disc_by_pid,
            rtp_out_of_order: now
                .rtp_out_of_order
                .saturating_sub(self.prev.rtp_out_of_order),
            udp_overflows: now.udp_overflows.saturating_sub(self.prev.udp_overflows),
        };

        self.prev = now;
        deltas
    }
}

/// Delta positivo por PID entre dois mapas de cumulativos.
///
/// PIDs sem novidade saem do mapa: o motor de checks trata chave ausente como
/// "sem violação" (é assim que um evento fecha quando o PID some do multiplex),
/// e uma entrada com zero significaria a mesma coisa gastando uma alocação.
///
/// RNF-PRB-003 — contador que regride (reset, reconexão) vira zero, não
/// underflow.
fn delta_by_pid(now: &HashMap<Pid, u64>, prev: &HashMap<Pid, u64>) -> HashMap<Pid, u64> {
    let mut out = HashMap::new();
    for (pid, total) in now {
        let before = prev.get(pid).copied().unwrap_or(0);
        let d = total.saturating_sub(before);
        if d > 0 {
            out.insert(*pid, d);
        }
    }
    out
}

/// Bitrate agregado dos PIDs de vídeo e de áudio do multiplex.
///
/// Alimenta os indicadores `V` e `A` do tile — **presença e bitrate**, nunca
/// nível de áudio (§8.1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AvPresence {
    pub video_kbps: f64,
    pub audio_kbps: f64,
    /// `true` se algum PID do multiplex está com `scrambling_control ≠ 0`.
    ///
    /// Preenchido pelo pipeline (badge `SCR` do tile), não por
    /// [`AvPresence::from_metrics`] — o `MetricsSnapshot` não carrega o campo.
    pub scrambled: bool,
    /// Altura do vídeo, quando conhecida — badge `HD`/`SD` do tile.
    ///
    /// Preenchido pelo pipeline a partir do Media Info, não por
    /// [`AvPresence::from_metrics`].
    pub video_height: Option<u32>,
}

impl AvPresence {
    /// Soma os bitrates por classificação de PID do snapshot.
    ///
    /// Só serve quando o produtor do snapshot classifica os PIDs; num feed do
    /// modo Probe o `MetricsAggregator` roda sem `TableDispatcher` e devolve
    /// `PidType::Unknown` para tudo — use
    /// [`AvPresence::from_metrics_with_pids`] nesse caso.
    ///
    /// SPEC-PROBE-018
    pub fn from_metrics(m: &MetricsSnapshot) -> Self {
        let mut out = Self::default();
        for entry in &m.pid_table {
            match entry.pid_type {
                PidType::Video { .. } => out.video_kbps += entry.bitrate_kbps,
                PidType::Audio { .. } => out.audio_kbps += entry.bitrate_kbps,
                _ => {}
            }
        }
        out
    }

    /// Soma os bitrates usando os PIDs vindos da PMT.
    ///
    /// SPEC-PROBE-018 — os conjuntos vazios significam "PMT ainda não
    /// recebida", **não** "sem vídeo": nesse caso a presença fica indefinida e
    /// o chamador deve omitir os checks `video_missing`/`audio_missing`, senão
    /// todo feed abriria um alarme crítico nos primeiros segundos.
    pub fn from_metrics_with_pids(
        m: &MetricsSnapshot,
        video_pids: &[Pid],
        audio_pids: &[Pid],
    ) -> Self {
        let mut out = Self::default();
        for entry in &m.pid_table {
            if video_pids.contains(&entry.pid) {
                out.video_kbps += entry.bitrate_kbps;
            } else if audio_pids.contains(&entry.pid) {
                out.audio_kbps += entry.bitrate_kbps;
            }
        }
        out
    }
}

/// Uma linha de `metrics.csv`.
///
/// SPEC-PROBE-005 · §6.1
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeSample {
    pub ts_utc: DateTime<Utc>,
    /// Segundos desde o início da sessão do feed.
    pub uptime_s: u64,
    pub connected: bool,
    pub bitrate_kbps: f64,
    pub null_ratio: f64,
    pub cc_errors_delta: u64,
    pub crc_errors_delta: u64,
    pub sync_loss_delta: u64,
    pub pcr_jitter_delta: u64,
    pub pcr_disc_delta: u64,
    /// Descartes atribuídos à própria probe neste segundo (SPEC-PROBE-013).
    pub local_drops_delta: u64,
    /// Atraso do tick em relação ao agendado, em ms (SPEC-PROBE-013).
    pub sched_jitter_ms: f64,
    /// Pior severidade aberta no instante da amostra (SPEC-PROBE-009).
    pub worst_severity: Option<Severity>,
    /// Camada IP do mesmo segundo; `None` antes do primeiro datagrama.
    ///
    /// spec-14 §6 — sai como colunas vazias, não como zeros.
    pub ip: Option<IpTick>,
}

impl ProbeSample {
    /// Serializa como uma linha de `metrics.csv`, sem quebra de linha.
    ///
    /// Formatação decimal com ponto e casas fixas: o arquivo é aberto no Excel
    /// (§6.1), e notação científica de `f64` em `to_string` estragaria a coluna.
    ///
    /// SPEC-PROBE-006
    pub fn to_csv(&self) -> String {
        let base = format!(
            "{},{},{},{:.1},{:.5},{},{},{},{},{},{},{:.1},{}",
            self.ts_utc.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            self.uptime_s,
            u8::from(self.connected),
            self.bitrate_kbps,
            self.null_ratio,
            self.cc_errors_delta,
            self.crc_errors_delta,
            self.sync_loss_delta,
            self.pcr_jitter_delta,
            self.pcr_disc_delta,
            self.local_drops_delta,
            self.sched_jitter_ms,
            self.worst_severity.map_or("", Severity::label),
        );
        format!("{base},{}", self.ip_columns())
    }

    /// As colunas da camada IP (spec-14 §6).
    ///
    /// Campo não observável ou não aplicável sai **vazio**, nunca como `0`:
    /// zero significa "medido e deu zero".  Num feed `Udp` puro toda a faixa
    /// `rtp_*`/`fec_*` sai vazia, e é isso que distingue "não medido" de
    /// "medido e sem perda" quando a planilha for aberta 12 h depois.
    fn ip_columns(&self) -> String {
        let Some(ip) = &self.ip else {
            // Colunas vazias: a aridade da linha não pode depender de haver ou
            // não datagrama no segundo, senão a planilha desalinha.
            return ",".repeat(IP_COLUMNS - 1);
        };

        let rtp = ip.rtp;
        let fec = &ip.fec;
        // `fec_present` só é afirmável quando a probe está de fato escutando as
        // portas de FEC; com `fec = off` a coluna fica vazia.
        let fec_present = if fec.listening {
            u8::from(fec.present).to_string()
        } else {
            String::new()
        };

        [
            encapsulation_label(ip.encapsulation).to_string(),
            ip.datagrams.to_string(),
            ip.bytes.to_string(),
            format!("{:.4}", ip.mbps),
            csv_opt_f64(ip.ts_per_datagram, 2),
            csv_opt(rtp.map(|r| r.received)),
            csv_opt(rtp.map(|r| r.missing)),
            csv_opt(rtp.map(|r| r.dup)),
            csv_opt(rtp.map(|r| r.reorder)),
            csv_opt(rtp.map(|r| r.too_old)),
            ip.loss_ratio.map_or(String::new(), |v| format!("{v:.9}")),
            ip.ssrc.map_or(String::new(), |s| format!("0x{s:08X}")),
            csv_opt_f64(ip.iat.min_us, 1),
            csv_opt_f64(ip.iat.avg_us, 1),
            csv_opt_f64(ip.iat.max_us, 1),
            csv_opt_f64(ip.iat.sd_us, 2),
            csv_opt_f64(ip.iat.p99_us, 1),
            csv_opt_f64(ip.iat_expected_us, 1),
            csv_opt_f64(ip.jitter_us, 2),
            fec_present,
            csv_opt(fec.l),
            csv_opt(fec.d),
            csv_opt_f64(fec.overhead_pct, 2),
            // Uma coluna só: a fonte esperada é uma.  Duas fontes viram evento
            // `multi_source`, e `source_count` é o que denuncia na planilha.
            ip.sources
                .first()
                .map_or(String::new(), |a| a.ip().to_string()),
            ip.sources.len().to_string(),
        ]
        .join(",")
    }
}

/// Quantas colunas a camada IP acrescenta à linha.
const IP_COLUMNS: usize = 25;

/// Rótulo estável do encapsulamento no CSV.
fn encapsulation_label(enc: Encapsulation) -> &'static str {
    match enc {
        Encapsulation::Unknown => "",
        Encapsulation::Udp => "udp",
        Encapsulation::Rtp => "rtp",
        Encapsulation::RtpFec => "rtp+fec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Cumulativos de bancada. O CRC entra atribuído a um PID de tabela porque
    /// é assim que ele chega do `ErrorSnapshot` — o total é derivado.
    fn counters(cc: &[(Pid, u64)], crc: u64, sync: u64) -> RawCounters {
        let crc_by_pid: HashMap<Pid, u64> = if crc > 0 {
            HashMap::from([(0x0000, crc)])
        } else {
            HashMap::new()
        };
        RawCounters {
            cc_errors_by_pid: cc.iter().copied().collect(),
            crc_errors: crc_by_pid.values().sum(),
            crc_by_pid,
            sync_losses: sync,
            ..Default::default()
        }
    }

    /// §5.3 — o primeiro tick após o reset não inventa delta.
    #[test]
    fn spec_probe_005_first_tick_yields_zero_deltas() {
        let mut base = CounterBaseline::new();
        let d = base.delta(counters(&[(100, 42)], 7, 3));
        assert_eq!(d, CounterDeltas::default());
    }

    /// §5.3 — deltas por PID são derivados dos cumulativos.
    #[test]
    fn spec_probe_005_delta_is_derived_per_pid() {
        let mut base = CounterBaseline::new();
        base.delta(counters(&[(100, 10), (200, 5)], 0, 0));

        let d = base.delta(counters(&[(100, 13), (200, 5), (300, 2)], 4, 1));
        assert_eq!(d.cc_by_pid.get(&100), Some(&3));
        assert_eq!(
            d.cc_by_pid.get(&200),
            None,
            "PID sem novos erros sai do mapa"
        );
        assert_eq!(d.cc_by_pid.get(&300), Some(&2));
        assert_eq!(d.cc_total, 5);
        assert_eq!(d.crc, 4);
        assert_eq!(d.sync_loss, 1);
    }

    /// RNF-PRB-003 — contador que regride (ResetErrors, reconexão) não faz
    /// underflow nem produz delta negativo.
    #[test]
    fn rnf_prb_003_counter_reset_does_not_underflow() {
        let mut base = CounterBaseline::new();
        base.delta(counters(&[(100, 1000)], 500, 9));
        let d = base.delta(counters(&[(100, 0)], 0, 0));
        assert_eq!(d.cc_total, 0);
        assert_eq!(d.crc, 0);
        assert_eq!(d.sync_loss, 0);
    }

    /// SPEC-PROBE-021 — CRC e PCR chegam atribuídos ao PID onde ocorreram, e
    /// não só como total do multiplex: sem isso a linha do PID na grade de
    /// saúde ficaria sempre verde num serviço que está de fato quebrado.
    #[test]
    fn spec_probe_021_crc_and_pcr_are_attributed_per_pid() {
        use ts::metrics::{PcrDiscontinuityRecord, PcrJitterRecord};

        let jitter = |pid: Pid| PcrJitterRecord {
            pid,
            timestamp: std::time::Instant::now(),
            expected_us: 0,
            measured_us: 0,
        };
        let disc = |pid: Pid| PcrDiscontinuityRecord {
            pid,
            timestamp: std::time::Instant::now(),
        };

        let mut m = MetricsSnapshot {
            pid_table: vec![],
            total_bitrate_kbps: 0.0,
            null_ratio: 0.0,
            errors: Default::default(),
            tdt_offset_secs: None,
            timestamp: std::time::Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
            pipeline: Default::default(),
        };
        // Duas tabelas no mesmo PID somam na mesma linha da grade.
        m.errors.crc_errors = HashMap::from([((0x0011, 0x42), 2), ((0x0011, 0x46), 1)]);
        m.errors.pcr_jitter_events = vec![jitter(0x0200), jitter(0x0200), jitter(0x0300)];
        m.errors.pcr_discontinuities = vec![disc(0x0200)];

        let mut base = CounterBaseline::new();
        base.delta(RawCounters::from_metrics(&m));

        m.errors.crc_errors.insert((0x0011, 0x42), 5);
        m.errors.pcr_jitter_events.push(jitter(0x0300));
        let d = base.delta(RawCounters::from_metrics(&m));

        assert_eq!(d.crc_by_pid.get(&0x0011), Some(&3));
        assert_eq!(d.crc, 3, "o total continua sendo a soma dos PIDs");
        assert_eq!(d.pcr_jitter_by_pid.get(&0x0300), Some(&1));
        assert_eq!(
            d.pcr_jitter_by_pid.get(&0x0200),
            None,
            "PID sem novidade sai do mapa"
        );
        assert_eq!(d.pcr_jitter, 1);
        assert_eq!(d.pcr_disc, 0);
    }

    /// SPEC-PROBE-011 — `reset()` faz o próximo tick virar o novo ponto zero.
    #[test]
    fn spec_probe_011_reset_rebaselines_without_burst() {
        let mut base = CounterBaseline::new();
        base.delta(counters(&[(100, 10)], 0, 0));
        base.reset();
        // Após a reconexão os contadores voltam do zero; nenhum delta deve
        // aparecer só por causa disso.
        let d = base.delta(counters(&[(100, 0)], 0, 0));
        assert_eq!(d.cc_total, 0);
        let d = base.delta(counters(&[(100, 2)], 0, 0));
        assert_eq!(d.cc_total, 2);
    }

    /// SPEC-PROBE-018 — a presença de A/V sai dos PIDs da PMT, não da
    /// classificação do aggregator (que num feed de Probe é sempre `Unknown`).
    #[test]
    fn spec_probe_018_presence_uses_pmt_pids() {
        let entry = |pid: Pid, kbps: f64| ts::metrics::PidEntry {
            pid,
            pid_type: PidType::Unknown,
            label: String::new(),
            bitrate_kbps: kbps,
            cc_errors: 0,
            packet_count: 0,
        };
        let m = MetricsSnapshot {
            pid_table: vec![entry(100, 14_000.0), entry(101, 192.0), entry(8191, 800.0)],
            total_bitrate_kbps: 15_000.0,
            null_ratio: 0.05,
            errors: Default::default(),
            tdt_offset_secs: None,
            timestamp: std::time::Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
            pipeline: Default::default(),
        };

        // Sem PMT, a classificação por tipo não enxerga nada.
        assert_eq!(AvPresence::from_metrics(&m).video_kbps, 0.0);

        let p = AvPresence::from_metrics_with_pids(&m, &[100], &[101]);
        assert!((p.video_kbps - 14_000.0).abs() < 1e-9);
        assert!((p.audio_kbps - 192.0).abs() < 1e-9);
    }

    /// §6.1 — a linha CSV tem exatamente as colunas do cabeçalho, na ordem.
    #[test]
    fn spec_probe_006_csv_row_matches_header_arity() {
        let s = ProbeSample {
            ts_utc: Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts"),
            uptime_s: 3661,
            connected: true,
            bitrate_kbps: 15002.4,
            null_ratio: 0.03125,
            cc_errors_delta: 3,
            crc_errors_delta: 0,
            sync_loss_delta: 0,
            pcr_jitter_delta: 1,
            pcr_disc_delta: 0,
            local_drops_delta: 0,
            sched_jitter_ms: 2.4,
            worst_severity: Some(Severity::Error),
            ip: None,
        };

        let row = s.to_csv();
        assert_eq!(
            row.split(',').count(),
            CSV_HEADER.split(',').count(),
            "linha e cabeçalho precisam ter a mesma aridade"
        );
        assert!(!row.contains('\n'));
        assert!(
            row.contains(",error,"),
            "a severidade continua na 13ª coluna"
        );
        assert!(row.contains(",15002.4,"), "bitrate sem notação científica");
        assert!(row.starts_with("2023-11-14T22:13:20.000Z,3661,1,"));
    }

    /// spec-14 §6 — a versão 2 acrescenta as colunas da camada IP **sem** mexer
    /// nas da camada base: uma planilha da versão 1 continua legível porque as
    /// 13 primeiras colunas não mudaram de nome nem de posição.
    #[test]
    fn spec_probe_ip_011_csv_v2_appends_without_moving_base_columns() {
        assert_eq!(CSV_SCHEMA_VERSION, 2);
        const V1_HEADER: &str = "ts_utc,uptime_s,connected,bitrate_kbps,null_ratio,\
cc_errors_delta,crc_errors_delta,sync_loss_delta,pcr_jitter_delta,pcr_disc_delta,\
local_drops_delta,sched_jitter_ms,worst_severity";
        assert!(
            CSV_HEADER.starts_with(V1_HEADER),
            "as colunas da versão 1 precisam continuar no mesmo lugar"
        );
        let v1_cols = V1_HEADER.split(',').count();
        assert_eq!(CSV_HEADER.split(',').count(), v1_cols + IP_COLUMNS);

        // As colunas anexadas são exatamente as do §6 da spec-14.
        let appended: Vec<&str> = CSV_HEADER.split(',').skip(v1_cols).collect();
        assert_eq!(
            appended,
            [
                "encapsulation",
                "ip_datagrams",
                "ip_bytes",
                "ip_mbps",
                "ts_per_datagram",
                "rtp_received",
                "rtp_missing_delta",
                "rtp_dup_delta",
                "rtp_reorder_delta",
                "rtp_too_old_delta",
                "rtp_loss_ratio",
                "ssrc",
                "iat_min_us",
                "iat_avg_us",
                "iat_max_us",
                "iat_sd_us",
                "iat_p99_us",
                "iat_expected_us",
                "rfc3550_jitter_us",
                "fec_present",
                "fec_l",
                "fec_d",
                "fec_overhead_pct",
                "source_ip",
                "source_count",
            ]
        );
    }

    /// spec-14 §6 — num feed UDP puro toda a faixa `rtp_*`/`fec_*` sai **vazia**,
    /// e não zerada: zero significaria "medido e sem perda", que é uma
    /// afirmação que ninguém verificou.
    #[test]
    fn spec_probe_ip_043_udp_row_leaves_rtp_and_fec_columns_empty() {
        let ip = IpTick {
            encapsulation: Encapsulation::Udp,
            datagrams: 1_400,
            bytes: 1_842_400,
            mbps: 14.7392,
            ts_per_datagram: Some(7.0),
            sources: vec!["10.0.0.9:50000".parse().expect("addr")],
            rtp: None,
            iat: net::IatSummary {
                count: 1_399,
                min_us: Some(690.0),
                avg_us: Some(701.9),
                max_us: Some(715.0),
                sd_us: Some(2.5),
                p50_us: Some(700.0),
                p95_us: Some(710.0),
                p99_us: Some(712.0),
                ..Default::default()
            },
            iat_expected_us: Some(701.9),
            ..Default::default()
        };
        let row = sample_with(Some(ip)).to_csv();
        let cols: Vec<&str> = row.split(',').collect();
        assert_eq!(cols.len(), CSV_HEADER.split(',').count());

        let col = |name: &str| {
            let idx = CSV_HEADER
                .split(',')
                .position(|c| c == name)
                .unwrap_or_else(|| panic!("coluna {name} não existe"));
            cols[idx]
        };

        assert_eq!(col("encapsulation"), "udp");
        assert_eq!(col("ip_datagrams"), "1400");
        assert_eq!(col("ts_per_datagram"), "7.00");
        assert_eq!(col("iat_avg_us"), "701.9");
        assert_eq!(col("source_ip"), "10.0.0.9");
        assert_eq!(col("source_count"), "1");
        for empty in [
            "rtp_received",
            "rtp_missing_delta",
            "rtp_dup_delta",
            "rtp_reorder_delta",
            "rtp_too_old_delta",
            "rtp_loss_ratio",
            "ssrc",
            "rfc3550_jitter_us",
            "fec_present",
            "fec_l",
            "fec_d",
            "fec_overhead_pct",
        ] {
            assert_eq!(col(empty), "", "{empty} deveria sair vazia num feed UDP");
        }
    }

    /// spec-14 §6 — num feed `RtpFec` as colunas de RTP e FEC saem preenchidas,
    /// e um zero medido continua sendo `0`.
    #[test]
    fn spec_probe_ip_011_rtp_fec_row_carries_every_measured_value() {
        let ip = IpTick {
            encapsulation: Encapsulation::RtpFec,
            datagrams: 1_400,
            bytes: 1_859_200,
            mbps: 14.8736,
            ts_per_datagram: Some(7.0),
            ssrc: Some(0x1A2B_3C4D),
            rtp: Some(crate::ip::RtpDelta {
                received: 1_400,
                missing: 0,
                dup: 0,
                reorder: 2,
                too_old: 0,
                source_restarts: 0,
                ssrc_changes: 0,
            }),
            loss_ratio: Some(0.0),
            jitter_us: Some(38.5),
            fec: crate::ip::FecStatus {
                present: true,
                listening: true,
                l: Some(8),
                d: Some(5),
                streams: 2,
                overhead_pct: Some(12.5),
                ssrc_mismatch: false,
                datagrams: 40,
            },
            ..Default::default()
        };
        let row = sample_with(Some(ip)).to_csv();
        let cols: Vec<&str> = row.split(',').collect();
        let col = |name: &str| {
            let idx = CSV_HEADER
                .split(',')
                .position(|c| c == name)
                .expect("coluna existe");
            cols[idx]
        };

        assert_eq!(col("encapsulation"), "rtp+fec");
        assert_eq!(col("rtp_received"), "1400");
        assert_eq!(col("rtp_missing_delta"), "0", "zero medido é zero");
        assert_eq!(col("rtp_reorder_delta"), "2");
        assert_eq!(col("rtp_loss_ratio"), "0.000000000");
        assert_eq!(col("ssrc"), "0x1A2B3C4D");
        assert_eq!(col("rfc3550_jitter_us"), "38.50");
        assert_eq!(col("fec_present"), "1");
        assert_eq!(col("fec_l"), "8");
        assert_eq!(col("fec_d"), "5");
        assert_eq!(col("fec_overhead_pct"), "12.50");
    }

    /// Amostra da camada base com a camada IP anexada.
    fn sample_with(ip: Option<IpTick>) -> ProbeSample {
        ProbeSample {
            ts_utc: Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts"),
            uptime_s: 10,
            connected: true,
            bitrate_kbps: 15_002.4,
            null_ratio: 0.03,
            cc_errors_delta: 0,
            crc_errors_delta: 0,
            sync_loss_delta: 0,
            pcr_jitter_delta: 0,
            pcr_disc_delta: 0,
            local_drops_delta: 0,
            sched_jitter_ms: 0.4,
            worst_severity: None,
            ip,
        }
    }

    /// §6.1 — sem severidade aberta, a coluna fica vazia (não "none").
    #[test]
    fn spec_probe_009_empty_severity_column_when_healthy() {
        let s = ProbeSample {
            ts_utc: Utc.timestamp_opt(0, 0).single().expect("ts"),
            uptime_s: 0,
            connected: false,
            bitrate_kbps: 0.0,
            null_ratio: 0.0,
            cc_errors_delta: 0,
            crc_errors_delta: 0,
            sync_loss_delta: 0,
            pcr_jitter_delta: 0,
            pcr_disc_delta: 0,
            local_drops_delta: 0,
            sched_jitter_ms: 0.0,
            worst_severity: None,
            ip: None,
        };
        let row = s.to_csv();
        assert!(
            row.contains(",0.0,,"),
            "severidade vazia, não \"none\": {row}"
        );
        assert_eq!(row.split(',').count(), CSV_HEADER.split(',').count());
    }
}
