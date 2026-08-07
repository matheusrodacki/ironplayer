//! Severidade e camada de um check.
//!
//! SPEC-PROBE-007 · SPEC-PROBE-009

use serde::{Deserialize, Serialize};

/// Severidade de um check, em ordem crescente de gravidade.
///
/// A ordenação derivada de `PartialOrd` é o que define "pior severidade do
/// bucket" na linha do tempo de saúde (SPEC-PROBE-009).
///
/// SPEC-PROBE-007
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Mudança de estado observável, sem impacto (PAT/PMT/codec/SSRC/IP de origem).
    Info,
    /// Fora do perfil, mas o stream continua utilizável (jitter, reorder, duplicata).
    Warning,
    /// Degradação real do conteúdo (perda RTP, CC error, CRC error, PCR error).
    Error,
    /// Stream inutilizável (feed indisponível, sync loss, PAT/PMT ausente).
    Critical,
}

impl Severity {
    /// Identificador estável usado em CSV, JSONL e relatório.
    ///
    /// SPEC-PROBE-006
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    /// Cor RGB (0xRRGGBB) da linha do tempo e dos indicadores do tile.
    ///
    /// SPEC-PROBE-009 · SPEC-PROBE-018
    pub fn rgb(self) -> u32 {
        match self {
            Self::Info => 0x5a_a0_d0,
            Self::Warning => 0xe8_94_3a,
            Self::Error => 0xd6_60_5f,
            Self::Critical => 0x8e_2c_2b,
        }
    }
}

/// Cor de um bucket **sem severidade** (tudo verde na janela).
///
/// SPEC-PROBE-009
pub const RGB_OK: u32 = 0x57_c0_8a;

/// Cor de um bucket **sem dado** — cinza, obrigatoriamente distinta de verde.
///
/// SPEC-PROBE-009 exige que "sem dado" não seja confundido com "sem erro".
pub const RGB_NO_DATA: u32 = 0x3a_43_4d;

/// Camada do pipeline à qual um check pertence.
///
/// SPEC-PROBE-007 define `Ip | Ts`; as demais existem para alimentar os
/// indicadores redondos do tile (§8.1: `IP RTP TS V A`) e o autodiagnóstico
/// da própria probe (SPEC-PROBE-013), sem exigir um segundo enum paralelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// Camada IP/UDP: inter-arrival, jitter, disponibilidade do datagrama.
    Ip,
    /// Camada RTP/FEC: perda, reordenação, duplicata, SSRC.
    Rtp,
    /// Camada Transport Stream: CC, CRC, sync loss, PCR, PSI.
    Ts,
    /// Presença/bitrate do PID de vídeo (não é análise perceptual).
    Video,
    /// Presença/bitrate do PID de áudio — **não** é nível de áudio (§8.1).
    Audio,
    /// Autodiagnóstico da probe: descartes locais, jitter de agendamento.
    Probe,
}

impl Layer {
    /// Rótulo curto do indicador redondo no tile do mosaico.
    ///
    /// SPEC-PROBE-018
    pub fn badge(self) -> &'static str {
        match self {
            Self::Ip => "IP",
            Self::Rtp => "RTP",
            Self::Ts => "TS",
            Self::Video => "V",
            Self::Audio => "A",
            Self::Probe => "PRB",
        }
    }

    /// Identificador estável para serialização e configuração.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::Rtp => "rtp",
            Self::Ts => "ts",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Probe => "probe",
        }
    }

    /// Camadas exibidas como indicadores no tile, na ordem do §8.1.
    ///
    /// SPEC-PROBE-018
    pub const TILE_ORDER: [Layer; 5] =
        [Layer::Ip, Layer::Rtp, Layer::Ts, Layer::Video, Layer::Audio];
}

/// Estado de saúde de uma camada num tile do mosaico.
///
/// SPEC-PROBE-018a: um check inaplicável ao encapsulamento do tile (RTP num
/// feed UDP puro) fica [`LayerHealth::NotApplicable`] — **nunca** verde, para
/// não afirmar que algo foi verificado quando não foi.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerHealth {
    /// Nenhum check da camada se aplica a este feed.
    #[default]
    NotApplicable,
    /// Camada avaliada, nenhum check aberto.
    Ok,
    /// Pior check aberto da camada.
    Degraded(Severity),
}

impl LayerHealth {
    /// Cor do indicador redondo.
    ///
    /// SPEC-PROBE-018
    pub fn rgb(self) -> u32 {
        match self {
            Self::NotApplicable => RGB_NO_DATA,
            Self::Ok => RGB_OK,
            Self::Degraded(sev) => sev.rgb(),
        }
    }

    /// Agrega uma severidade observada, mantendo a pior.
    pub fn worsen(&mut self, sev: Severity) {
        *self = match *self {
            Self::Degraded(cur) if cur >= sev => Self::Degraded(cur),
            _ => Self::Degraded(sev),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-009 — a ordenação de severidade define o "pior do bucket".
    #[test]
    fn spec_probe_009_severity_is_ordered_by_gravity() {
        assert!(Severity::Critical > Severity::Error);
        assert!(Severity::Error > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);

        let bucket = [Severity::Info, Severity::Error, Severity::Warning];
        assert_eq!(bucket.iter().copied().max(), Some(Severity::Error));
    }

    /// SPEC-PROBE-009 — "sem dado" é cinza e nunca igual a verde.
    #[test]
    fn spec_probe_009_no_data_color_differs_from_ok() {
        assert_ne!(RGB_NO_DATA, RGB_OK);
    }

    /// SPEC-PROBE-018a — camada inaplicável não vira verde.
    #[test]
    fn spec_probe_018a_not_applicable_is_not_green() {
        assert_ne!(LayerHealth::NotApplicable.rgb(), RGB_OK);
        assert_eq!(LayerHealth::Ok.rgb(), RGB_OK);
    }

    /// SPEC-PROBE-018 — `worsen` mantém a pior severidade observada.
    #[test]
    fn spec_probe_018_worsen_keeps_worst() {
        let mut h = LayerHealth::Ok;
        h.worsen(Severity::Warning);
        h.worsen(Severity::Critical);
        h.worsen(Severity::Info);
        assert_eq!(h, LayerHealth::Degraded(Severity::Critical));
    }
}
