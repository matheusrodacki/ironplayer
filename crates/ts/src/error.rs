/// Tipos de erro do crate `ts`.
///
/// SPEC-TS-001 · SPEC-TS-002 · SPEC-TS-003 · SPEC-TS-004
use thiserror::Error;

use crate::Pid;

/// Erros retornados pelo parsing e processamento de pacotes MPEG-TS.
///
/// SPEC-TS-001
#[derive(Debug, Error)]
pub enum TsError {
    /// Byte de sincronização do pacote TS não é 0x47.
    ///
    /// SPEC-TS-001
    #[error("sync byte inválido: esperado 0x47, encontrado 0x{0:02X}")]
    InvalidSyncByte(u8),

    /// O slice fornecido não tem exatamente 188 bytes.
    ///
    /// SPEC-TS-001
    #[error("tamanho de pacote inválido: esperado 188 bytes, encontrado {0}")]
    InvalidPacketSize(usize),

    /// O campo `section_length` da seção PSI/SI excede o máximo legal (4093).
    ///
    /// SPEC-TS-003
    #[error("section_length {0} excede o máximo legal de 4093")]
    SectionTooLarge(u16),

    /// O Adaptation Field do pacote é malformado (truncado ou tamanho incoerente).
    ///
    /// SPEC-TS-001
    #[error("adaptation field malformado")]
    MalformedAdaptationField,
}

/// Eventos emitidos pelo `TsDemuxer` para o canal de métricas/diagnóstico.
///
/// SPEC-TS-002 · SPEC-TS-003
#[derive(Debug, Clone)]
pub enum TsEvent {
    /// Erro de Continuity Counter detectado em um PID.
    ///
    /// SPEC-TS-002b
    CcError { pid: Pid, expected: u8, got: u8 },

    /// Byte de sincronização perdido; demuxer avançou no buffer para re-sincronizar.
    ///
    /// SPEC-TS-002c
    SyncLost { bytes_skipped: usize },

    /// CRC-32 inválido em uma seção PSI/SI montada.
    ///
    /// SPEC-TS-003b
    CrcError { pid: Pid, table_id: u8 },

    /// Pacote processado; carrega pid e tamanho (188 bytes) para cálculo de bitrate.
    ///
    /// SPEC-METRICS-001
    Packet { pid: Pid, bytes: usize },
}

/// Eventos emitidos pelo `PcrTracker` para diagnóstico de jitter e descontinuidade.
///
/// SPEC-TS-004b
#[derive(Debug, Clone)]
pub enum PcrEvent {
    /// Jitter PCR acima do threshold (500 µs por padrão).
    ///
    /// SPEC-TS-004b
    Jitter {
        pid: Pid,
        expected_us: i64,
        measured_us: i64,
    },

    /// Descontinuidade detectada no PCR de um PID.
    ///
    /// SPEC-TS-004b
    Discontinuity {
        pid: Pid,
        reason: DiscontinuityReason,
    },
}

/// Razão de uma descontinuidade PCR.
///
/// SPEC-TS-004b
#[derive(Debug, Clone)]
pub enum DiscontinuityReason {
    /// O flag `discontinuity_indicator` estava setado no Adaptation Field.
    Flag,
    /// Salto de PCR acima do threshold (100 ms por padrão) sem flag explícito.
    LargeJump { delta_ms: u64 },
}
