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

/// Cor de um check **não aplicável** ao encapsulamento ou ao backend.
///
/// SPEC-PROBE-IP-049 — "não aplicável" e "sem dado" são coisas diferentes e
/// precisam de tratamento visual distinto: uma célula de escopo que ainda não
/// existia não é a mesma coisa que um check que nunca vai ser avaliado neste
/// feed.  Antes da spec-14 as duas usavam [`RGB_NO_DATA`], e a grade dizia
/// "cinza" para os dois casos sem meio de distingui-los.
pub const RGB_NOT_APPLICABLE: u32 = 0x2a_2f_3a;

/// Linha da grade de saúde (ou painel) em que os eventos de uma camada
/// aparecem.
///
/// SPEC-PROBE-IP-050 — o mapa camada → superfície precisa ser **total**: um
/// check cuja camada não caia em lugar nenhum existe no `events.jsonl` e nunca
/// aparece na grade, ou seja, fica invisível justamente no artefato que se olha
/// depois de 12 h.  É uma falha silenciosa, por isso tem teste.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthRow {
    /// `IP / UDP` — ou só `IP` num feed UDP puro (SPEC-PROBE-IP-048).
    Network,
    /// `RTP / FEC` — existe só quando o feed tem RTP.
    Rtp,
    /// `TRANSPORTE`.
    Transport,
    /// Linhas de serviço e de PID: presença de vídeo e de áudio.
    Content,
    /// Painel "Saúde da probe" (§8.2) — autodiagnóstico não é saúde do sinal e
    /// não disputa espaço com ele na grade.
    ProbeHealth,
}

impl HealthRow {
    /// Rótulo da linha na grade.
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "IP / UDP",
            Self::Rtp => "RTP / FEC",
            Self::Transport => "TRANSPORTE",
            Self::Content => "CONTEÚDO",
            Self::ProbeHealth => "SAÚDE DA PROBE",
        }
    }

    /// Camadas cujos eventos caem nesta linha.
    pub fn layers(self) -> &'static [Layer] {
        match self {
            Self::Network => &[Layer::Ip],
            Self::Rtp => &[Layer::Rtp],
            Self::Transport => &[Layer::Ts],
            Self::Content => &[Layer::Video, Layer::Audio],
            Self::ProbeHealth => &[Layer::Probe],
        }
    }
}

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

    /// Todas as camadas — usado pelo teste de cobertura da grade.
    ///
    /// SPEC-PROBE-IP-050
    pub const ALL: [Layer; 6] = [
        Layer::Ip,
        Layer::Rtp,
        Layer::Ts,
        Layer::Video,
        Layer::Audio,
        Layer::Probe,
    ];

    /// Onde os eventos desta camada aparecem.
    ///
    /// SPEC-PROBE-IP-050
    pub fn health_row(self) -> HealthRow {
        match self {
            Self::Ip => HealthRow::Network,
            Self::Rtp => HealthRow::Rtp,
            Self::Ts => HealthRow::Transport,
            Self::Video | Self::Audio => HealthRow::Content,
            Self::Probe => HealthRow::ProbeHealth,
        }
    }
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
            Self::NotApplicable => RGB_NOT_APPLICABLE,
            Self::Ok => RGB_OK,
            Self::Degraded(sev) => sev.rgb(),
        }
    }

    /// Rótulo do tooltip — é ele que diz qual cinza é qual.
    ///
    /// SPEC-PROBE-IP-049
    pub fn label(self) -> &'static str {
        match self {
            Self::NotApplicable => "n/a",
            Self::Ok => "ok",
            Self::Degraded(sev) => sev.label(),
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

    /// SPEC-PROBE-018a · SPEC-PROBE-IP-049 — inaplicável não é verde **nem**
    /// "sem dado": são três estados distintos e a UI precisa poder separá-los.
    #[test]
    fn spec_probe_ip_049_not_applicable_differs_from_ok_and_from_no_data() {
        assert_ne!(LayerHealth::NotApplicable.rgb(), RGB_OK);
        assert_ne!(LayerHealth::NotApplicable.rgb(), RGB_NO_DATA);
        assert_ne!(RGB_NO_DATA, RGB_OK);
        assert_eq!(LayerHealth::Ok.rgb(), RGB_OK);
        assert_eq!(LayerHealth::NotApplicable.label(), "n/a");
        assert_eq!(
            LayerHealth::Degraded(Severity::Error).label(),
            Severity::Error.label()
        );
    }

    /// SPEC-PROBE-IP-050 — o mapa camada → superfície é total: nenhuma camada
    /// fica sem lugar onde aparecer.
    #[test]
    fn spec_probe_ip_050_every_layer_has_a_surface() {
        for layer in Layer::ALL {
            let row = layer.health_row();
            assert!(
                row.layers().contains(&layer),
                "{} não aparece na linha que declara representá-lo",
                layer.label()
            );
            assert!(!row.label().is_empty());
        }
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
