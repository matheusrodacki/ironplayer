//! Tipos base e monitores para o sistema de métricas MPEG-TS.
//!
//! Define os tipos de dados imutáveis usados por `BitrateMonitor`, `ErrorTracker`
//! e `MetricsSnapshot` para rastreamento de bitrate por PID e contadores de erros.
//!
//! SPEC-METRICS-001 · SPEC-METRICS-002 · SPEC-METRICS-003

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::Pid;

// ---------------------------------------------------------------------------
// Codec enumerations
// ---------------------------------------------------------------------------

/// Codec de vídeo identificado via PMT (stream_type).
///
/// SPEC-METRICS-001
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    /// H.264 / AVC (stream_type 0x1B)
    H264,
    /// H.265 / HEVC (stream_type 0x24)
    H265,
    /// MPEG-2 Video (stream_type 0x02)
    Mpeg2,
    /// Codec de vídeo não reconhecido; carrega o `stream_type` original.
    Unknown(u8),
}

/// Codec de áudio identificado via PMT (stream_type).
///
/// SPEC-METRICS-001
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC (stream_type 0x0F)
    Aac,
    /// AC-3 / Dolby Digital (stream_type 0x81 ou descriptor 0x6A)
    Ac3,
    /// E-AC-3 / Dolby Digital Plus (stream_type 0x87)
    Eac3,
    /// MPEG-1 Layer II Audio (stream_type 0x04)
    MpegAudio,
    /// Codec de áudio não reconhecido; carrega o `stream_type` original.
    Unknown(u8),
}

// ---------------------------------------------------------------------------
// PID classification
// ---------------------------------------------------------------------------

/// Classificação funcional de um PID MPEG-TS.
///
/// SPEC-METRICS-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidType {
    /// Program Association Table (PID 0x0000)
    Pat,
    /// Program Map Table
    Pmt,
    /// Network Information Table (PID 0x0010)
    Nit,
    /// Service Description Table (PID 0x0011)
    Sdt,
    /// Event Information Table (PID 0x0012)
    Eit,
    /// Time and Date Table (PID 0x0014)
    Tdt,
    /// Bouquet Association Table (PID 0x0011, quando BAT)
    Bat,
    /// PID de vídeo elementar com codec identificado.
    Video { codec: VideoCodec },
    /// PID de áudio elementar com codec identificado.
    Audio { codec: AudioCodec },
    /// PID dedicado somente à transmissão de PCR (sem payload ES).
    Pcr,
    /// Null packets — padding do multiplex (PID 0x1FFF).
    NullPacket,
    /// PID observado mas ainda não classificado.
    Unknown,
}

// ---------------------------------------------------------------------------
// Bitrate entry
// ---------------------------------------------------------------------------

/// Entrada de bitrate por PID produzida pelo `BitrateMonitor::snapshot()`.
///
/// SPEC-METRICS-001c
#[derive(Debug, Clone, PartialEq)]
pub struct PidBitrateEntry {
    /// Identificador do PID (0x0000–0x1FFF).
    pub pid: Pid,
    /// Bitrate médio na janela deslizante em kbps.
    pub bitrate_kbps: f64,
    /// Contagem acumulada de pacotes na janela atual.
    pub packet_count: u64,
}

// ---------------------------------------------------------------------------
// PID table entry (snapshot completo)
// ---------------------------------------------------------------------------

/// Entrada da tabela de PIDs no `MetricsSnapshot`.
///
/// Agrega bitrate, tipo, label legível e contadores de erro para um único PID.
///
/// SPEC-METRICS-001 · SPEC-METRICS-002
#[derive(Debug, Clone)]
pub struct PidEntry {
    /// Identificador do PID (0x0000–0x1FFF).
    pub pid: Pid,
    /// Classificação funcional do PID.
    pub pid_type: PidType,
    /// Nome legível (ex.: "Video H.264 — Serviço 1", "PAT", "Null").
    pub label: String,
    /// Bitrate médio na janela deslizante em kbps.
    pub bitrate_kbps: f64,
    /// Total de Continuity Counter errors observados neste PID.
    pub cc_errors: u64,
    /// Contagem acumulada de pacotes na janela atual.
    pub packet_count: u64,
}

// ---------------------------------------------------------------------------
// Error records (para PCR jitter e descontinuidade)
// ---------------------------------------------------------------------------

/// Registro de evento de jitter PCR acima do threshold.
///
/// SPEC-METRICS-002
#[derive(Debug, Clone)]
pub struct PcrJitterRecord {
    /// PID do PCR onde o jitter foi detectado.
    pub pid: Pid,
    /// Instante em que o evento foi registrado.
    pub timestamp: Instant,
    /// Valor esperado de PCR em microssegundos.
    pub expected_us: i64,
    /// Valor medido de PCR em microssegundos.
    pub measured_us: i64,
}

/// Registro de evento de descontinuidade PCR.
///
/// SPEC-METRICS-002
#[derive(Debug, Clone)]
pub struct PcrDiscontinuityRecord {
    /// PID do PCR onde a descontinuidade foi detectada.
    pub pid: Pid,
    /// Instante em que o evento foi registrado.
    pub timestamp: Instant,
}

// ---------------------------------------------------------------------------
// ErrorSnapshot — instantâneo imutável dos contadores de erro
// ---------------------------------------------------------------------------

/// Instantâneo imutável dos contadores de erro coletados pelo `ErrorTracker`.
///
/// Produzido por `ErrorTracker::snapshot()` e embutido no `MetricsSnapshot`.
/// Mudanças posteriores no `ErrorTracker` não afetam instâncias existentes.
///
/// SPEC-METRICS-002a
#[derive(Debug, Clone, Default)]
pub struct ErrorSnapshot {
    /// Contagem de CC errors por PID.
    pub cc_errors: HashMap<Pid, u64>,
    /// Eventos de jitter PCR acima do threshold (limitado a `max_error_log_entries`).
    pub pcr_jitter_events: Vec<PcrJitterRecord>,
    /// Eventos de descontinuidade PCR (limitado a `max_error_log_entries`).
    pub pcr_discontinuities: Vec<PcrDiscontinuityRecord>,
    /// Contagem de erros de CRC por `(pid, table_id)`.
    pub crc_errors: HashMap<(Pid, u8), u64>,
    /// Total de eventos de perda de sincronismo TS.
    pub sync_losses: u64,
    /// Total de pacotes RTP recebidos fora de ordem.
    pub rtp_out_of_order: u64,
    /// Total de overflows do buffer UDP.
    pub udp_overflows: u64,
}

impl ErrorSnapshot {
    /// Retorna o total de CC errors somados em todos os PIDs.
    ///
    /// SPEC-METRICS-002b
    pub fn total_cc_errors(&self) -> u64 {
        self.cc_errors.values().sum()
    }
}

// ---------------------------------------------------------------------------
// MetricsSnapshot — snapshot global publicado a 1 Hz
// ---------------------------------------------------------------------------

/// Instantâneo global das métricas do pipeline, publicado a cada 1 segundo.
///
/// Distribuído pela UI via `tokio::sync::watch`. É `Clone` — a UI clona o valor
/// ao ler o canal; mudanças posteriores no pipeline não afetam instâncias anteriores.
///
/// Os campos `av_sync_offset_ms`, `late_frames_dropped`, `early_frames_held`,
/// `pts_discontinuities` e `video_queue_depth` são preenchidos pela camada de
/// UI (crates/ui) a partir da `VideoQueue` e do clock master — o aggregator de
/// métricas `ts` os inicializa em zero; a UI os sobrescreve após cada ciclo.
///
/// SPEC-METRICS-003 · SPEC-METRICS-SYNC-001
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    /// Tabela de PIDs ativos, ordenada por bitrate decrescente.
    ///
    /// SPEC-METRICS-001c
    pub pid_table: Vec<PidEntry>,
    /// Bitrate total do multiplex em kbps (inclui null packets).
    ///
    /// SPEC-METRICS-001d
    pub total_bitrate_kbps: f64,
    /// Proporção de bytes de null packets no total (0.0–1.0).
    ///
    /// SPEC-METRICS-001e
    pub null_ratio: f64,
    /// Contadores de erro no momento do snapshot.
    ///
    /// SPEC-METRICS-002a
    pub errors: ErrorSnapshot,
    /// Offset UTC derivado do TDT/TOT, em segundos desde a época Unix.
    /// `None` se nenhum TDT foi recebido ainda.
    pub tdt_offset_secs: Option<i64>,
    /// Instante de criação deste snapshot.
    pub timestamp: Instant,
    // ── Campos de sincronização A/V ──────────────────────────────────────────
    /// Offset atual de sincronização A/V em milissegundos.
    ///
    /// Positivo = vídeo adiantado em relação ao áudio (frame_pts > clock_pts).
    /// Negativo = vídeo atrasado. Zero enquanto o pipeline de vídeo está inativo.
    ///
    /// SPEC-METRICS-SYNC-001
    pub av_sync_offset_ms: i32,
    /// Total acumulado de frames de vídeo descartados por chegada tardia
    /// (PTS < clock − DROP_PTS).
    ///
    /// SPEC-METRICS-SYNC-001
    pub late_frames_dropped: u64,
    /// Total acumulado de chamadas `pop_ready` que retornaram `TooEarly`
    /// (PTS > clock + HOLD_PTS).
    ///
    /// SPEC-METRICS-SYNC-001
    pub early_frames_held: u64,
    /// Total acumulado de descontinuidades de PTS detectadas pela `VideoQueue`
    /// (salto de PTS > RESYNC_PTS).
    ///
    /// SPEC-METRICS-SYNC-001
    pub pts_discontinuities: u64,
    /// Número de frames atualmente na `VideoQueue` (profundidade instantânea).
    ///
    /// SPEC-METRICS-SYNC-001
    pub video_queue_depth: u16,
    /// Métricas do pipeline de decodificação e renderização GPU.
    ///
    /// SPEC-METRICS-PIPELINE-001
    pub pipeline: PipelineMetrics,
}

// ---------------------------------------------------------------------------
// PipelineMetrics — métricas do pipeline de decode e renderização GPU
// ---------------------------------------------------------------------------

/// Métricas do pipeline de decodificação FFmpeg e renderização GPU.
///
/// Preenchidas pela camada `av`/`ui`:
/// - Campos do decoder (`decoder_threads_used`, `deinterlacer_active`,
///   `decode_time_ms_p50/p99`) são escritos pela thread `av-decode` via
///   `Arc<RwLock<PipelineMetrics>>` e copiados no ciclo de poll da UI.
/// - Campos do renderer (`gpu_upload_bytes_per_sec`, `colorspace`,
///   `color_range`) são atualizados inline em `poll_video_frames` a partir
///   do `VideoRenderer`.
///
/// SPEC-METRICS-PIPELINE-001
#[derive(Debug, Clone, Default)]
pub struct PipelineMetrics {
    /// Número de threads de decodificação em uso.
    ///
    /// Igual a `CodecConfig::thread_count` resolvido; 0 antes da primeira
    /// decodificação.
    pub decoder_threads_used: u32,
    /// `true` quando o deinterlacador bwdif está ativo em pelo menos um PID.
    pub deinterlacer_active: bool,
    /// Latência de decode P50 (mediana) por PID de vídeo, em milissegundos.
    ///
    /// Calculado sobre janela deslizante dos últimos 100 frames por PID.
    pub decode_time_ms_p50: HashMap<Pid, f64>,
    /// Latência de decode P99 por PID de vídeo, em milissegundos.
    ///
    /// Calculado sobre janela deslizante dos últimos 100 frames por PID.
    pub decode_time_ms_p99: HashMap<Pid, f64>,
    /// Taxa de bytes de planos YUV enviados à GPU por segundo.
    ///
    /// Atualizado a cada frame; valor médio no intervalo de 1 s.
    pub gpu_upload_bytes_per_sec: u64,
    /// Espaço de cor do frame de vídeo mais recente.
    ///
    /// Valores possíveis: `"BT.709"`, `"BT.601"`, `"BT.2020"`, `"Unspecified"`.
    /// `None` antes do primeiro frame.
    pub colorspace: Option<String>,
    /// Faixa de cor (range) do frame de vídeo mais recente.
    ///
    /// Valores possíveis: `"Limited"`, `"Full"`. `None` antes do primeiro frame.
    pub color_range: Option<String>,
    // ── Hardware acceleration (Sprint 2, SPEC-METRICS-HW-001) ────────────────
    /// `true` enquanto o decoder está produzindo frames acelerados em GPU.
    ///
    /// SPEC-METRICS-HW-001
    pub hw_decode_active: bool,
    /// Identificador do codec hwaccel ativo (ex.: `"hevc_d3d11va"`).
    ///
    /// SPEC-METRICS-HW-001
    pub hw_decode_codec: Option<String>,
    /// Razão registrada quando o caminho hwaccel caiu para CPU.
    ///
    /// `None` quando não houve fallback ou quando hwaccel não foi solicitado.
    ///
    /// SPEC-METRICS-HW-001
    pub hw_decode_fallback_reason: Option<String>,
    /// Frames atualmente em uso no pool D3D11VA (Frames "ocupados").
    ///
    /// SPEC-METRICS-HW-001
    pub hw_frame_pool_in_use: u32,
    /// Eventos de TDR (Timeout Detection and Recovery) tratados na sessão.
    ///
    /// SPEC-METRICS-HW-001
    pub tdr_recoveries: u64,
    /// Nome legível do adapter GPU em uso (ex.: `"NVIDIA GeForce RTX 4060"`).
    ///
    /// SPEC-METRICS-HW-001
    pub gpu_adapter_name: Option<String>,
    /// LUID do adapter GPU em uso, codificado como u64.
    ///
    /// SPEC-METRICS-HW-001
    pub gpu_adapter_luid: u64,
}

impl Default for MetricsSnapshot {
    fn default() -> Self {
        Self {
            pid_table: Vec::new(),
            total_bitrate_kbps: 0.0,
            null_ratio: 0.0,
            errors: ErrorSnapshot::default(),
            tdt_offset_secs: None,
            timestamp: Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
            pipeline: PipelineMetrics::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// BitrateMonitor — janela deslizante por PID (SPEC-METRICS-001)
// ---------------------------------------------------------------------------

/// Monitor de bitrate com janela deslizante por PID.
///
/// Mantém um `VecDeque<(Instant, usize)>` por PID para calcular bitrate médio
/// sobre uma janela de tempo configurável. Entradas fora da janela são expurgadas
/// a cada `update()` e filtradas virtualmente em todas as leituras.
///
/// SPEC-METRICS-001
pub struct BitrateMonitor {
    /// Tamanho da janela deslizante.
    window: Duration,
    /// Amostras de bytes por PID, ordenadas cronologicamente.
    pids: HashMap<Pid, VecDeque<(Instant, usize)>>,
}

impl BitrateMonitor {
    /// Cria um novo `BitrateMonitor` com a janela deslizante especificada.
    ///
    /// SPEC-METRICS-001
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pids: HashMap::new(),
        }
    }

    /// Registra `bytes` recebidos para `pid` no instante atual.
    ///
    /// Remove do início da fila todas as entradas anteriores ao corte
    /// `now - window`, mantendo a memória limitada à janela ativa.
    ///
    /// SPEC-METRICS-001a
    pub fn update(&mut self, pid: Pid, bytes: usize) {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let deque = self.pids.entry(pid).or_default();
        deque.push_back((now, bytes));
        while let Some(&(ts, _)) = deque.front() {
            if ts < cutoff {
                deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// Retorna o bitrate médio em kbps para `pid` na janela atual.
    ///
    /// Retorna `0.0` se o PID nunca foi visto ou não possui entradas na janela.
    ///
    /// SPEC-METRICS-001b
    pub fn bitrate_kbps(&self, pid: Pid) -> f64 {
        let cutoff = Instant::now().checked_sub(self.window);
        let sum_bytes: usize = self
            .pids
            .get(&pid)
            .map(|d| {
                d.iter()
                    .filter(|(ts, _)| cutoff.map_or(true, |c| *ts >= c))
                    .map(|(_, b)| b)
                    .sum()
            })
            .unwrap_or(0);
        (sum_bytes as f64) * 8.0 / self.window.as_secs_f64() / 1000.0
    }

    /// Retorna snapshot de todos os PIDs com bitrate > 0, ordenados por bitrate
    /// decrescente.
    ///
    /// SPEC-METRICS-001c
    pub fn snapshot(&self) -> Vec<PidBitrateEntry> {
        let cutoff = Instant::now().checked_sub(self.window);
        let mut entries: Vec<PidBitrateEntry> = self
            .pids
            .iter()
            .filter_map(|(&pid, deque)| {
                let (sum_bytes, count) = deque
                    .iter()
                    .filter(|(ts, _)| cutoff.map_or(true, |c| *ts >= c))
                    .fold((0usize, 0u64), |(s, c), (_, b)| (s + b, c + 1));
                if sum_bytes == 0 {
                    None
                } else {
                    let bitrate_kbps =
                        (sum_bytes as f64) * 8.0 / self.window.as_secs_f64() / 1000.0;
                    Some(PidBitrateEntry {
                        pid,
                        bitrate_kbps,
                        packet_count: count,
                    })
                }
            })
            .collect();
        entries.sort_by(|a, b| {
            b.bitrate_kbps
                .partial_cmp(&a.bitrate_kbps)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries
    }

    /// Retorna a soma do bitrate de todos os PIDs (incluindo null packets) em kbps.
    ///
    /// SPEC-METRICS-001d
    pub fn total_bitrate_kbps(&self) -> f64 {
        let cutoff = Instant::now().checked_sub(self.window);
        let sum_bytes: usize = self
            .pids
            .values()
            .flat_map(|d| {
                d.iter()
                    .filter(move |(ts, _)| cutoff.map_or(true, |c| *ts >= c))
                    .map(|(_, b)| b)
            })
            .sum();
        (sum_bytes as f64) * 8.0 / self.window.as_secs_f64() / 1000.0
    }

    /// Retorna a proporção de bytes de null packets (PID 0x1FFF) no total (0.0–1.0).
    ///
    /// Retorna `0.0` se não houve tráfego na janela.
    ///
    /// SPEC-METRICS-001e
    pub fn null_packet_ratio(&self) -> f64 {
        let cutoff = Instant::now().checked_sub(self.window);
        let filter = |ts: &Instant| cutoff.map_or(true, |c| *ts >= c);
        let null_bytes: usize = self
            .pids
            .get(&0x1FFF)
            .map(|d| d.iter().filter(|(ts, _)| filter(ts)).map(|(_, b)| b).sum())
            .unwrap_or(0);
        let total_bytes: usize = self
            .pids
            .values()
            .flat_map(|d| d.iter().filter(|(ts, _)| filter(ts)).map(|(_, b)| b))
            .sum();
        if total_bytes == 0 {
            0.0
        } else {
            null_bytes as f64 / total_bytes as f64
        }
    }
}

// ---------------------------------------------------------------------------
// ErrorTracker — acumulador de contadores de erro (SPEC-METRICS-002)
// ---------------------------------------------------------------------------

/// Acumulador de contadores de erro do pipeline MPEG-TS / RTP / UDP.
///
/// Todos os métodos de registro são `&mut self` — sem concorrência interna;
/// o chamador sincroniza o acesso quando necessário.
///
/// O log de eventos PCR (`pcr_jitter_events`, `pcr_discontinuities`) é
/// limitado a `max_error_log_entries` para evitar crescimento ilimitado de memória.
///
/// SPEC-METRICS-002
pub struct ErrorTracker {
    /// Contagem de CC errors por PID.
    cc_errors: HashMap<Pid, u64>,
    /// Eventos de jitter PCR (limitado a `max_error_log_entries`).
    pcr_jitter_events: Vec<PcrJitterRecord>,
    /// Eventos de descontinuidade PCR (limitado a `max_error_log_entries`).
    pcr_discontinuities: Vec<PcrDiscontinuityRecord>,
    /// Contagem de erros de CRC por `(pid, table_id)`.
    crc_errors: HashMap<(Pid, u8), u64>,
    /// Total de eventos de perda de sincronismo TS.
    sync_losses: u64,
    /// Total de pacotes RTP recebidos fora de ordem.
    rtp_out_of_order: u64,
    /// Total de overflows do buffer UDP.
    udp_overflows: u64,
    /// Capacidade máxima dos vetores de log de eventos PCR.
    max_error_log_entries: usize,
}

impl ErrorTracker {
    /// Cria um novo `ErrorTracker` com todos os contadores zerados.
    ///
    /// `max_error_log_entries` limita o tamanho dos vetores de log de eventos
    /// PCR (`pcr_jitter_events` e `pcr_discontinuities`). Quando o limite é
    /// atingido, novas entradas são silenciosamente descartadas.
    ///
    /// SPEC-METRICS-002
    pub fn new(max_error_log_entries: usize) -> Self {
        Self {
            cc_errors: HashMap::new(),
            pcr_jitter_events: Vec::new(),
            pcr_discontinuities: Vec::new(),
            crc_errors: HashMap::new(),
            sync_losses: 0,
            rtp_out_of_order: 0,
            udp_overflows: 0,
            max_error_log_entries,
        }
    }

    /// Incrementa o contador de CC errors para `pid`.
    ///
    /// SPEC-METRICS-002a
    pub fn record_cc_error(&mut self, pid: Pid) {
        *self.cc_errors.entry(pid).or_insert(0) += 1;
    }

    /// Registra um evento de jitter PCR.
    ///
    /// Se o log já atingiu `max_error_log_entries`, a entrada é descartada.
    ///
    /// SPEC-METRICS-002b
    pub fn record_pcr_jitter(&mut self, record: PcrJitterRecord) {
        if self.pcr_jitter_events.len() < self.max_error_log_entries {
            self.pcr_jitter_events.push(record);
        }
    }

    /// Registra um evento de descontinuidade PCR.
    ///
    /// Se o log já atingiu `max_error_log_entries`, a entrada é descartada.
    ///
    /// SPEC-METRICS-002b
    pub fn record_pcr_discontinuity(&mut self, record: PcrDiscontinuityRecord) {
        if self.pcr_discontinuities.len() < self.max_error_log_entries {
            self.pcr_discontinuities.push(record);
        }
    }

    /// Incrementa o contador de erros de CRC para `(pid, table_id)`.
    ///
    /// SPEC-METRICS-002b
    pub fn record_crc_error(&mut self, pid: Pid, table_id: u8) {
        *self.crc_errors.entry((pid, table_id)).or_insert(0) += 1;
    }

    /// Incrementa o contador de perdas de sincronismo TS.
    ///
    /// SPEC-METRICS-002b
    pub fn record_sync_loss(&mut self) {
        self.sync_losses += 1;
    }

    /// Incrementa o contador de pacotes RTP recebidos fora de ordem.
    ///
    /// SPEC-METRICS-002c
    pub fn record_rtp_out_of_order(&mut self) {
        self.rtp_out_of_order += 1;
    }

    /// Incrementa o contador de overflows do buffer UDP.
    ///
    /// SPEC-METRICS-002c
    pub fn record_udp_overflow(&mut self) {
        self.udp_overflows += 1;
    }

    /// Zera todos os contadores e limpa os logs de eventos PCR.
    ///
    /// SPEC-METRICS-002c
    pub fn reset(&mut self) {
        self.cc_errors.clear();
        self.pcr_jitter_events.clear();
        self.pcr_discontinuities.clear();
        self.crc_errors.clear();
        self.sync_losses = 0;
        self.rtp_out_of_order = 0;
        self.udp_overflows = 0;
    }

    /// Produz um `ErrorSnapshot` imutável com cópia dos contadores atuais.
    ///
    /// Operações futuras sobre o `ErrorTracker` não afetam o snapshot retornado.
    ///
    /// SPEC-METRICS-002a
    pub fn snapshot(&self) -> ErrorSnapshot {
        ErrorSnapshot {
            cc_errors: self.cc_errors.clone(),
            pcr_jitter_events: self.pcr_jitter_events.clone(),
            pcr_discontinuities: self.pcr_discontinuities.clone(),
            crc_errors: self.crc_errors.clone(),
            sync_losses: self.sync_losses,
            rtp_out_of_order: self.rtp_out_of_order,
            udp_overflows: self.udp_overflows,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-METRICS-001 — `VideoCodec` e `AudioCodec` são Clone e PartialEq.
    #[test]
    fn spec_metrics_001_codecs_clone_eq() {
        let v = VideoCodec::H264;
        assert_eq!(v.clone(), VideoCodec::H264);

        let a = AudioCodec::Aac;
        assert_eq!(a.clone(), AudioCodec::Aac);

        let v_unk = VideoCodec::Unknown(0xAB);
        assert_eq!(v_unk.clone(), VideoCodec::Unknown(0xAB));
        assert_ne!(v_unk, VideoCodec::Unknown(0x01));
    }

    /// SPEC-METRICS-001 — `PidType` é Clone e PartialEq.
    #[test]
    fn spec_metrics_001_pid_type_clone_eq() {
        let t = PidType::Video {
            codec: VideoCodec::H265,
        };
        assert_eq!(
            t.clone(),
            PidType::Video {
                codec: VideoCodec::H265
            }
        );
        assert_ne!(
            t,
            PidType::Audio {
                codec: AudioCodec::Aac
            }
        );
        assert_eq!(PidType::NullPacket.clone(), PidType::NullPacket);
    }

    /// SPEC-METRICS-001c — `PidBitrateEntry` é Clone e PartialEq.
    #[test]
    fn spec_metrics_001_pid_bitrate_entry_clone() {
        let entry = PidBitrateEntry {
            pid: 0x0100,
            bitrate_kbps: 3.0,
            packet_count: 2,
        };
        let cloned = entry.clone();
        assert_eq!(cloned.pid, entry.pid);
        assert_eq!(cloned.bitrate_kbps, entry.bitrate_kbps);
        assert_eq!(cloned.packet_count, entry.packet_count);
    }

    /// SPEC-METRICS-002a — `ErrorSnapshot` é Clone; alterações posteriores
    /// no mapa original não afetam o snapshot.
    #[test]
    fn spec_metrics_002a_error_snapshot_immutable() {
        let snap = ErrorSnapshot {
            cc_errors: HashMap::from([(0x100u16, 3u64)]),
            pcr_jitter_events: vec![],
            pcr_discontinuities: vec![],
            crc_errors: HashMap::new(),
            sync_losses: 0,
            rtp_out_of_order: 0,
            udp_overflows: 0,
        };
        let mut snap2 = snap.clone();
        snap2.cc_errors.insert(0x100, 99);
        // snap permanece inalterado
        assert_eq!(*snap.cc_errors.get(&0x100).unwrap(), 3);
    }

    /// SPEC-METRICS-002b — `total_cc_errors` soma corretamente.
    #[test]
    fn spec_metrics_002b_total_cc_errors() {
        let snap = ErrorSnapshot {
            cc_errors: HashMap::from([(0x100u16, 3u64), (0x200u16, 7u64)]),
            pcr_jitter_events: vec![],
            pcr_discontinuities: vec![],
            crc_errors: HashMap::new(),
            sync_losses: 0,
            rtp_out_of_order: 0,
            udp_overflows: 0,
        };
        assert_eq!(snap.total_cc_errors(), 10);
    }

    /// SPEC-METRICS-003 — `MetricsSnapshot` é Clone.
    #[test]
    fn spec_metrics_003_metrics_snapshot_clone() {
        let snap = MetricsSnapshot {
            pid_table: vec![],
            total_bitrate_kbps: 12.5,
            null_ratio: 0.1,
            errors: ErrorSnapshot {
                cc_errors: HashMap::new(),
                pcr_jitter_events: vec![],
                pcr_discontinuities: vec![],
                crc_errors: HashMap::new(),
                sync_losses: 0,
                rtp_out_of_order: 0,
                udp_overflows: 0,
            },
            tdt_offset_secs: Some(1_716_000_000),
            timestamp: Instant::now(),
            av_sync_offset_ms: 0,
            late_frames_dropped: 0,
            early_frames_held: 0,
            pts_discontinuities: 0,
            video_queue_depth: 0,
            pipeline: PipelineMetrics::default(),
        };
        let cloned = snap.clone();
        assert_eq!(cloned.total_bitrate_kbps, snap.total_bitrate_kbps);
        assert_eq!(cloned.null_ratio, snap.null_ratio);
        assert_eq!(cloned.tdt_offset_secs, snap.tdt_offset_secs);
    }

    /// SPEC-METRICS-SYNC-001 — campos de sync A/V têm defaults corretos e são clonable.
    #[test]
    fn spec_metrics_sync_001_sync_fields_defaults_and_clone() {
        let snap = MetricsSnapshot::default();
        assert_eq!(snap.av_sync_offset_ms, 0);
        assert_eq!(snap.late_frames_dropped, 0);
        assert_eq!(snap.early_frames_held, 0);
        assert_eq!(snap.pts_discontinuities, 0);
        assert_eq!(snap.video_queue_depth, 0);

        // Campos podem ser preenchidos e clonados corretamente.
        let mut snap2 = MetricsSnapshot::default();
        snap2.av_sync_offset_ms = -12;
        snap2.late_frames_dropped = 3;
        snap2.early_frames_held = 7;
        snap2.pts_discontinuities = 1;
        snap2.video_queue_depth = 4;
        let cloned = snap2.clone();
        assert_eq!(cloned.av_sync_offset_ms, -12);
        assert_eq!(cloned.late_frames_dropped, 3);
        assert_eq!(cloned.early_frames_held, 7);
        assert_eq!(cloned.pts_discontinuities, 1);
        assert_eq!(cloned.video_queue_depth, 4);
    }

    // -----------------------------------------------------------------------
    // BitrateMonitor tests (SPEC-METRICS-001a–001e)
    // -----------------------------------------------------------------------

    /// SPEC-METRICS-001a — `update` registra bytes; PID desconhecido retorna 0.
    #[test]
    fn spec_metrics_001a_update_registers_bytes() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(1));
        assert_eq!(mon.bitrate_kbps(0x100), 0.0);
        mon.update(0x100, 188);
        assert!(mon.bitrate_kbps(0x100) > 0.0);
    }

    /// SPEC-METRICS-001b — 188 bytes na janela de 1 s = 1.504 kbps.
    #[test]
    fn spec_metrics_001b_single_packet_bitrate() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(1));
        mon.update(0x100, 188);
        let kbps = mon.bitrate_kbps(0x100);
        // 188 * 8 / 1.0 / 1000 = 1.504
        assert!((kbps - 1.504).abs() < 1e-9, "kbps={kbps}");
    }

    /// SPEC-METRICS-001 — janela deslizante expira entradas antigas.
    #[test]
    fn spec_metrics_001_bitrate_window_sliding() {
        let mut mon = BitrateMonitor::new(Duration::from_millis(80));
        mon.update(0x100, 10_000);
        // Aguarda a janela expirar
        std::thread::sleep(Duration::from_millis(120));
        // Leitura virtual filtra entrada expirada
        assert_eq!(mon.bitrate_kbps(0x100), 0.0);
        let snap = mon.snapshot();
        assert!(snap.is_empty(), "snapshot deve estar vazio após expiração");
    }

    /// SPEC-METRICS-001 — entradas expiradas são removidas pela `update`.
    #[test]
    fn spec_metrics_001_update_evicts_expired_entries() {
        let mut mon = BitrateMonitor::new(Duration::from_millis(50));
        mon.update(0x200, 500);
        std::thread::sleep(Duration::from_millis(80));
        // Chamada de update deve remover entrada expirada
        mon.update(0x200, 100);
        // Apenas o último pacote (100 bytes) deve estar na janela
        let expected = (100.0f64) * 8.0 / 0.05 / 1000.0;
        let kbps = mon.bitrate_kbps(0x200);
        assert!(
            (kbps - expected).abs() < 0.5,
            "kbps={kbps}, esperado≈{expected}"
        );
    }

    /// SPEC-METRICS-001c — snapshot ordenado por bitrate decrescente.
    #[test]
    fn spec_metrics_001c_snapshot_ordered_desc() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(2));
        mon.update(0x300, 100); // menor
        mon.update(0x100, 1000); // maior
        mon.update(0x200, 500); // médio
        let snap = mon.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(snap[0].bitrate_kbps >= snap[1].bitrate_kbps);
        assert!(snap[1].bitrate_kbps >= snap[2].bitrate_kbps);
        assert_eq!(snap[0].pid, 0x100);
        assert_eq!(snap[2].pid, 0x300);
    }

    /// SPEC-METRICS-001c — snapshot contém `packet_count` correto.
    #[test]
    fn spec_metrics_001c_snapshot_packet_count() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(2));
        mon.update(0x100, 188);
        mon.update(0x100, 188);
        mon.update(0x100, 188);
        let snap = mon.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].packet_count, 3);
    }

    /// SPEC-METRICS-001d — `total_bitrate_kbps` inclui null packets (PID 0x1FFF).
    #[test]
    fn spec_metrics_001d_total_includes_null() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(1));
        mon.update(0x100, 376); // 2 pacotes × 188
        mon.update(0x1FFF, 188); // null packet
        let total = mon.total_bitrate_kbps();
        let expected = (376.0 + 188.0) * 8.0 / 1.0 / 1000.0;
        assert!(
            (total - expected).abs() < 1e-9,
            "total={total}, esperado={expected}"
        );
    }

    /// SPEC-METRICS-001e — `null_packet_ratio` em intervalo 0.0–1.0 e valor correto.
    #[test]
    fn spec_metrics_001e_null_ratio() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(1));
        mon.update(0x100, 376); // 376 bytes ES
        mon.update(0x1FFF, 188); // 188 bytes null
                                 // total = 564; null_ratio = 188/564 ≈ 0.3333
        let ratio = mon.null_packet_ratio();
        assert!((0.0..=1.0).contains(&ratio), "ratio fora do range: {ratio}");
        let expected = 188.0 / 564.0;
        assert!(
            (ratio - expected).abs() < 1e-9,
            "ratio={ratio}, esperado={expected}"
        );
    }

    /// SPEC-METRICS-001e — `null_packet_ratio` retorna 0.0 sem tráfego.
    #[test]
    fn spec_metrics_001e_null_ratio_zero_when_empty() {
        let mon = BitrateMonitor::new(Duration::from_secs(1));
        assert_eq!(mon.null_packet_ratio(), 0.0);
    }

    /// SPEC-METRICS-001e — `null_packet_ratio` retorna 0.0 sem null packets.
    #[test]
    fn spec_metrics_001e_null_ratio_zero_no_null_pid() {
        let mut mon = BitrateMonitor::new(Duration::from_secs(1));
        mon.update(0x100, 188);
        assert_eq!(mon.null_packet_ratio(), 0.0);
    }

    // -----------------------------------------------------------------------
    // ErrorTracker tests (SPEC-METRICS-002a–002c)
    // -----------------------------------------------------------------------

    /// SPEC-METRICS-002a — `snapshot` produz cópia imutável; alterações posteriores
    /// no tracker não afetam o snapshot.
    #[test]
    fn spec_metrics_002a_snapshot_is_immutable() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_cc_error(0x100);
        let snap = tracker.snapshot();
        // Após o snapshot, registrar mais erros não deve alterar snap
        tracker.record_cc_error(0x100);
        tracker.record_cc_error(0x100);
        assert_eq!(*snap.cc_errors.get(&0x100u16).unwrap(), 1);
        assert_eq!(*tracker.snapshot().cc_errors.get(&0x100u16).unwrap(), 3);
    }

    /// SPEC-METRICS-002a — `record_cc_error` acumula por PID corretamente.
    #[test]
    fn spec_metrics_002a_cc_error_per_pid() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_cc_error(0x100);
        tracker.record_cc_error(0x100);
        tracker.record_cc_error(0x200);
        let snap = tracker.snapshot();
        assert_eq!(*snap.cc_errors.get(&0x100u16).unwrap(), 2);
        assert_eq!(*snap.cc_errors.get(&0x200u16).unwrap(), 1);
        assert_eq!(snap.total_cc_errors(), 3);
    }

    /// SPEC-METRICS-002b — `record_pcr_jitter` armazena evento no log.
    #[test]
    fn spec_metrics_002b_pcr_jitter_recorded() {
        let mut tracker = ErrorTracker::new(10);
        let rec = PcrJitterRecord {
            pid: 0x0101,
            timestamp: Instant::now(),
            expected_us: 1_000_000,
            measured_us: 1_000_500,
        };
        tracker.record_pcr_jitter(rec);
        let snap = tracker.snapshot();
        assert_eq!(snap.pcr_jitter_events.len(), 1);
        assert_eq!(snap.pcr_jitter_events[0].pid, 0x0101);
        assert_eq!(snap.pcr_jitter_events[0].expected_us, 1_000_000);
        assert_eq!(snap.pcr_jitter_events[0].measured_us, 1_000_500);
    }

    /// SPEC-METRICS-002b — log de jitter é limitado a `max_error_log_entries`.
    #[test]
    fn spec_metrics_002b_pcr_jitter_log_bounded() {
        let max = 5;
        let mut tracker = ErrorTracker::new(max);
        for i in 0..10u8 {
            tracker.record_pcr_jitter(PcrJitterRecord {
                pid: i as u16,
                timestamp: Instant::now(),
                expected_us: 0,
                measured_us: i as i64,
            });
        }
        let snap = tracker.snapshot();
        assert_eq!(snap.pcr_jitter_events.len(), max);
        // Apenas os primeiros `max` eventos foram retidos
        assert_eq!(snap.pcr_jitter_events[0].pid, 0);
        assert_eq!(snap.pcr_jitter_events[max - 1].pid, (max - 1) as u16);
    }

    /// SPEC-METRICS-002b — `record_pcr_discontinuity` armazena evento e respeita limite.
    #[test]
    fn spec_metrics_002b_pcr_discontinuity_bounded() {
        let max = 3;
        let mut tracker = ErrorTracker::new(max);
        for _ in 0..5 {
            tracker.record_pcr_discontinuity(PcrDiscontinuityRecord {
                pid: 0x0200,
                timestamp: Instant::now(),
            });
        }
        let snap = tracker.snapshot();
        assert_eq!(snap.pcr_discontinuities.len(), max);
    }

    /// SPEC-METRICS-002b — `record_crc_error` acumula por `(pid, table_id)`.
    #[test]
    fn spec_metrics_002b_crc_error_per_pid_table() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_crc_error(0x0000, 0x00); // PAT
        tracker.record_crc_error(0x0000, 0x00);
        tracker.record_crc_error(0x0010, 0x40); // NIT
        let snap = tracker.snapshot();
        assert_eq!(*snap.crc_errors.get(&(0x0000u16, 0x00u8)).unwrap(), 2);
        assert_eq!(*snap.crc_errors.get(&(0x0010u16, 0x40u8)).unwrap(), 1);
    }

    /// SPEC-METRICS-002b — `record_sync_loss` incrementa contador.
    #[test]
    fn spec_metrics_002b_sync_loss_counter() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_sync_loss();
        tracker.record_sync_loss();
        assert_eq!(tracker.snapshot().sync_losses, 2);
    }

    /// SPEC-METRICS-002c — `record_rtp_out_of_order` incrementa contador.
    #[test]
    fn spec_metrics_002c_rtp_out_of_order_counter() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_rtp_out_of_order();
        tracker.record_rtp_out_of_order();
        tracker.record_rtp_out_of_order();
        assert_eq!(tracker.snapshot().rtp_out_of_order, 3);
    }

    /// SPEC-METRICS-002c — `record_udp_overflow` incrementa contador.
    #[test]
    fn spec_metrics_002c_udp_overflow_counter() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_udp_overflow();
        assert_eq!(tracker.snapshot().udp_overflows, 1);
    }

    /// SPEC-METRICS-002c — `reset` zera todos os contadores e limpa os logs.
    #[test]
    fn spec_metrics_002c_reset_zeroes_counters() {
        let mut tracker = ErrorTracker::new(100);
        tracker.record_cc_error(0x100);
        tracker.record_sync_loss();
        tracker.record_rtp_out_of_order();
        tracker.record_udp_overflow();
        tracker.record_crc_error(0x0000, 0x00);
        tracker.record_pcr_jitter(PcrJitterRecord {
            pid: 0x0101,
            timestamp: Instant::now(),
            expected_us: 0,
            measured_us: 1,
        });
        tracker.record_pcr_discontinuity(PcrDiscontinuityRecord {
            pid: 0x0101,
            timestamp: Instant::now(),
        });
        tracker.reset();
        let snap = tracker.snapshot();
        assert!(snap.cc_errors.is_empty());
        assert!(snap.pcr_jitter_events.is_empty());
        assert!(snap.pcr_discontinuities.is_empty());
        assert!(snap.crc_errors.is_empty());
        assert_eq!(snap.sync_losses, 0);
        assert_eq!(snap.rtp_out_of_order, 0);
        assert_eq!(snap.udp_overflows, 0);
        assert_eq!(snap.total_cc_errors(), 0);
    }

    /// SPEC-METRICS-002 — tracker novo tem todos os contadores zerados.
    #[test]
    fn spec_metrics_002_new_tracker_is_zeroed() {
        let tracker = ErrorTracker::new(50);
        let snap = tracker.snapshot();
        assert!(snap.cc_errors.is_empty());
        assert!(snap.pcr_jitter_events.is_empty());
        assert!(snap.pcr_discontinuities.is_empty());
        assert!(snap.crc_errors.is_empty());
        assert_eq!(snap.sync_losses, 0);
        assert_eq!(snap.rtp_out_of_order, 0);
        assert_eq!(snap.udp_overflows, 0);
    }
}
