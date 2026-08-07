//! Inventário de serviços de um MPTS.
//!
//! O modo Probe começou medindo o **multiplex inteiro**: um feed, um conjunto
//! de contadores.  Num MPTS isso responde "o transporte está bom?", mas não
//! "qual serviço está ruim?" — e é essa a pergunta que o operador faz quando
//! olha o mosaico.  Este módulo é o vocabulário mínimo para responder: quais
//! serviços existem no feed, quais PIDs pertencem a cada um e o que cada PID é.
//!
//! O inventário é montado fora daqui (em `src/feed.rs`, a partir de PAT/PMT/SDT)
//! e entregue ao [`crate::ProbeEngine`] a cada tick.  O crate `probe` não
//! parseia PSI — §5.1.
//!
//! SPEC-PROBE-021 · SPEC-PROBE-022

use ts::Pid;

use crate::event::EventContext;
use crate::severity::Layer;
use crate::snapshot::SnapshotState;

/// Papel de um elementary stream dentro do serviço.
///
/// A classificação vem do `stream_type` da PMT (mais os descriptors, quando o
/// `stream_type` sozinho é ambíguo — 0x06 é áudio AC-3 ou legenda dependendo do
/// descriptor).  Quem resolve essa ambiguidade é o crate `ts`; aqui só chega o
/// veredito.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    /// Dados, teletexto, AIT, carrossel — tudo que não é A/V nem legenda.
    Data,
    /// PID que só carrega PCR, sem elementary stream próprio.
    Pcr,
    #[default]
    Other,
}

impl StreamKind {
    /// Rótulo curto usado na grade de saúde e na lista de PIDs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Video => "vídeo",
            Self::Audio => "áudio",
            Self::Subtitle => "legenda",
            Self::Data => "dados",
            Self::Pcr => "pcr",
            Self::Other => "outro",
        }
    }

    /// Camada de saúde correspondente, quando existe.
    ///
    /// Legenda e dados **não** têm camada própria: um PID de teletexto sem
    /// bitrate não é um alarme de vídeo nem de áudio, e inventar uma camada
    /// para ele só criaria indicador que ninguém sabe interpretar.
    pub fn layer(self) -> Option<Layer> {
        match self {
            Self::Video => Some(Layer::Video),
            Self::Audio => Some(Layer::Audio),
            _ => None,
        }
    }
}

/// Um elementary stream de um serviço.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceStream {
    pub pid: Pid,
    /// `stream_type` da PMT (ISO 13818-1 Table 2-36).
    pub stream_type: u8,
    pub kind: StreamKind,
    /// Rótulo do codec vindo de `ts::tables::PmtStream::label`.
    pub codec: String,
    /// ISO-639 do `iso_639_language_descriptor` (tag 0x0A), quando presente.
    pub language: Option<String>,
}

impl ServiceStream {
    /// Rótulo da linha na grade de saúde: `H.264 Video · por (6100)`.
    ///
    /// SPEC-PROBE-023
    pub fn describe(&self) -> String {
        let mut label = if self.codec.trim().is_empty() {
            self.kind.label().to_string()
        } else {
            self.codec.clone()
        };
        if let Some(lang) = &self.language {
            label.push_str(" · ");
            label.push_str(lang);
        }
        format!("{label} ({})", self.pid)
    }
}

/// Um serviço do multiplex.
///
/// SPEC-PROBE-021
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceInfo {
    pub service_id: u16,
    /// Nome vindo da SDT; ausente até a SDT chegar (ou num SPTS sem SDT).
    pub name: Option<String>,
    pub provider: Option<String>,
    pub pmt_pid: Pid,
    pub pcr_pid: Pid,
    /// `free_CA_mode` da SDT — presença de CA, nunca tentativa de decifrar.
    pub scrambled: bool,
    pub streams: Vec<ServiceStream>,
}

impl ServiceInfo {
    /// Nome exibido: o da SDT, ou `Serviço {id}` enquanto ela não chega.
    ///
    /// Nunca devolve string vazia — um tile sem rótulo no mosaico de serviços
    /// é indistinguível de um slot livre.
    ///
    /// SPEC-PROBE-022
    pub fn display_name(&self) -> String {
        match self.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("Serviço {}", self.service_id),
        }
    }

    /// PIDs de um tipo dentro do serviço.
    pub fn pids_of(&self, kind: StreamKind) -> impl Iterator<Item = Pid> + '_ {
        self.streams
            .iter()
            .filter(move |s| s.kind == kind)
            .map(|s| s.pid)
    }

    /// PID de vídeo primário — o que o thumbnail decodifica.
    ///
    /// SPEC-PROBE-024
    pub fn primary_video_pid(&self) -> Option<Pid> {
        self.streams
            .iter()
            .find(|s| s.kind == StreamKind::Video)
            .map(|s| s.pid)
    }

    /// `stream_type` do PID de vídeo primário, para armar o decoder.
    ///
    /// SPEC-PROBE-024
    pub fn primary_video_stream_type(&self) -> Option<u8> {
        self.streams
            .iter()
            .find(|s| s.kind == StreamKind::Video)
            .map(|s| s.stream_type)
    }

    /// `true` se o PID pertence a este serviço.
    pub fn contains_pid(&self, pid: Pid) -> bool {
        self.streams.iter().any(|s| s.pid == pid)
    }

    /// `true` se a ocorrência pertence a este serviço.
    ///
    /// SPEC-PROBE-021
    pub fn owns(&self, ctx: &EventContext) -> bool {
        service_owns(
            self.service_id,
            self.pmt_pid,
            self.streams.iter().map(|s| s.pid),
            ctx,
        )
    }
}

/// Resultado do último tick de snapshot de vídeo de **um serviço**.
///
/// O thumbnail deixou de ser por feed: num MPTS, um quadro só não diz qual dos
/// serviços está no ar (SPEC-PROBE-024).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceVisual {
    /// Altura nativa do vídeo — badge `HD`/`SD` do tile de serviço.
    pub video_height: Option<u32>,
    pub state: SnapshotState,
}

/// Inventário de serviços de um feed, na ordem da PAT.
pub type ServiceInventory = Vec<ServiceInfo>;

/// Regra única de "esta ocorrência é deste serviço".
///
/// Existe como função livre porque tanto o inventário ([`ServiceInfo`]) quanto
/// o estado publicado (`ServiceSnapshot`) precisam dela, e duas cópias da regra
/// divergiriam no primeiro ajuste.
///
/// SPEC-PROBE-021
pub fn service_owns(
    service_id: u16,
    pmt_pid: Pid,
    mut pids: impl Iterator<Item = Pid>,
    ctx: &EventContext,
) -> bool {
    if ctx.service_id == Some(service_id) {
        return true;
    }
    match ctx.pid {
        Some(pid) => pid == pmt_pid || pids.any(|p| p == pid),
        None => false,
    }
}

/// Serviço dono de um PID, se houver.
///
/// Um PID compartilhado entre serviços (raro, mas legal no DVB) é atribuído ao
/// **primeiro** da lista: a alternativa — contar o erro em todos — inflaria a
/// contagem agregada e faria a soma dos serviços não bater com o multiplex.
///
/// SPEC-PROBE-021
pub fn owner_of(inventory: &[ServiceInfo], pid: Pid) -> Option<u16> {
    inventory
        .iter()
        .find(|s| s.contains_pid(pid) || s.pmt_pid == pid)
        .map(|s| s.service_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(id: u16, name: Option<&str>) -> ServiceInfo {
        ServiceInfo {
            service_id: id,
            name: name.map(str::to_string),
            pmt_pid: 0x0100 + id,
            pcr_pid: 0x0200 + id,
            streams: vec![
                ServiceStream {
                    pid: 0x0200 + id,
                    stream_type: 0x1B,
                    kind: StreamKind::Video,
                    codec: "H.264 Video".into(),
                    language: None,
                },
                ServiceStream {
                    pid: 0x0201 + id,
                    stream_type: 0x81,
                    kind: StreamKind::Audio,
                    codec: "AC-3 Audio".into(),
                    language: Some("por".into()),
                },
            ],
            ..Default::default()
        }
    }

    /// SPEC-PROBE-022 — sem SDT o serviço ainda tem rótulo utilizável.
    #[test]
    fn spec_probe_022_display_name_falls_back_to_service_id() {
        assert_eq!(service(1, None).display_name(), "Serviço 1");
        assert_eq!(service(1, Some("  ")).display_name(), "Serviço 1");
        assert_eq!(service(1, Some("GLOBO_RJ")).display_name(), "GLOBO_RJ");
    }

    /// SPEC-PROBE-023 — a linha da grade identifica codec, idioma e PID.
    #[test]
    fn spec_probe_023_stream_label_carries_codec_language_and_pid() {
        let svc = service(1, None);
        assert_eq!(svc.streams[0].describe(), "H.264 Video (513)");
        assert_eq!(svc.streams[1].describe(), "AC-3 Audio · por (514)");

        let bare = ServiceStream {
            pid: 700,
            kind: StreamKind::Data,
            ..Default::default()
        };
        assert_eq!(bare.describe(), "dados (700)");
    }

    /// SPEC-PROBE-024 — o thumbnail arma o PID de vídeo primário do serviço.
    #[test]
    fn spec_probe_024_primary_video_pid_is_the_first_video_stream() {
        let svc = service(1, None);
        assert_eq!(svc.primary_video_pid(), Some(513));
        assert_eq!(svc.primary_video_stream_type(), Some(0x1B));

        let audio_only = ServiceInfo {
            streams: vec![ServiceStream {
                pid: 300,
                kind: StreamKind::Audio,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(audio_only.primary_video_pid(), None);
    }

    /// SPEC-PROBE-021 — um PID pertence a um serviço só, mesmo quando aparece
    /// em duas PMTs: contá-lo duas vezes faria a soma dos serviços estourar o
    /// total do multiplex.
    #[test]
    fn spec_probe_021_pid_owner_is_the_first_service_that_lists_it() {
        let mut a = service(1, None);
        let mut b = service(2, None);
        b.streams[0].pid = a.streams[0].pid;
        let inventory = vec![a.clone(), b];

        assert_eq!(owner_of(&inventory, a.streams[0].pid), Some(1));
        assert_eq!(owner_of(&inventory, a.streams[1].pid), Some(1));
        assert_eq!(owner_of(&inventory, a.pmt_pid), Some(1));
        assert_eq!(owner_of(&inventory, 0x1FFF), None);

        a.streams.clear();
        assert!(!a.contains_pid(513));
    }

    /// SPEC-PROBE-023 — legenda e dados não viram indicador de camada.
    #[test]
    fn spec_probe_023_only_av_streams_map_to_a_layer() {
        assert_eq!(StreamKind::Video.layer(), Some(Layer::Video));
        assert_eq!(StreamKind::Audio.layer(), Some(Layer::Audio));
        assert_eq!(StreamKind::Subtitle.layer(), None);
        assert_eq!(StreamKind::Data.layer(), None);
    }
}
