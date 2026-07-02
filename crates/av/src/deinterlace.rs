//! Deinterlacing: bwdif (CPU) e D3D11 Video Processor (GPU).
//!
//! O backend ativo depende de [`DeinterlaceProfile`] e da detecção de scan type.
//!
//! # Invariantes bwdif (não regredir — L-003)
//!
//! 1. Buffer source do grafo declara `colorspace` e `range`.
//! 2. PTS de saída passa por [`crate::ffi::rescale_bwdif_output_pts`] (÷2).
//!
//! SPEC-AV-005 · SPEC-AV-006

use std::sync::Arc;

use crate::error::AvError;
use crate::ffi::{
    frame_color_range, frame_colorspace, frame_format, frame_height, frame_width,
    FfmpegFilterGraph, FfmpegFrame, FfmpegLib, FilterLib,
};

/// Backend de deinterlace ativo para um PID de vídeo.
///
/// SPEC-AV-006
pub(crate) enum DeinterlaceBackend {
    /// Nenhum processamento de deinterlace.
    None,
    /// bwdif via libavfilter (perfil Quality).
    Bwdif(Deinterlacer),
    /// D3D11 Video Processor (perfil Performance).
    #[cfg(windows)]
    D3d11Vp(crate::hw::D3d11VideoProcessor),
}

impl DeinterlaceBackend {
    /// Limpa qualquer estado de deinterlace.
    pub(crate) fn clear(&mut self) {
        *self = Self::None;
    }

    /// Rótulo para telemetria (`"bwdif"`, `"D3D11 VP"`, ou `None`).
    pub(crate) fn label(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Bwdif(_) => Some("bwdif"),
            #[cfg(windows)]
            Self::D3d11Vp(_) => Some("D3D11 VP"),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Retorna referência mutável ao bwdif, se ativo.
    pub(crate) fn bwdif_mut(&mut self) -> Option<&mut Deinterlacer> {
        match self {
            Self::Bwdif(di) => Some(di),
            _ => None,
        }
    }
}

/// Deinterlacador baseado em bwdif da libavfilter.
///
/// SPEC-AV-005
pub(crate) struct Deinterlacer {
    filter_lib: Arc<FilterLib>,
    ffmpeg_lib: Arc<FfmpegLib>,
    graph: Option<FfmpegFilterGraph>,
    /// Parâmetros do grafo atual: `(width, height, pix_fmt, colorspace, range)`.
    graph_dims: Option<(u32, u32, i32, i32, i32)>,
    /// Quando `true`, o grafo usa `deint=all`.
    deint_all: bool,
}

impl Deinterlacer {
    /// Cria um novo `Deinterlacer` sem grafo ativo.
    ///
    /// SPEC-AV-005
    pub(crate) fn new(
        filter_lib: Arc<FilterLib>,
        ffmpeg_lib: Arc<FfmpegLib>,
        deint_all: bool,
    ) -> Self {
        Self {
            filter_lib,
            ffmpeg_lib,
            graph: None,
            graph_dims: None,
            deint_all,
        }
    }

    /// Processa um frame através do bwdif.
    ///
    /// SPEC-AV-005
    pub(crate) fn process(&mut self, frame: &FfmpegFrame) -> Result<Option<FfmpegFrame>, AvError> {
        let (width, height, pix_fmt, colorspace, color_range) = unsafe {
            (
                frame_width(frame.as_ptr()) as u32,
                frame_height(frame.as_ptr()) as u32,
                frame_format(frame.as_ptr()),
                frame_colorspace(frame.as_ptr()),
                frame_color_range(frame.as_ptr()),
            )
        };

        let dims = (width, height, pix_fmt, colorspace, color_range);
        if self.graph_dims != Some(dims) {
            tracing::debug!(
                width,
                height,
                pix_fmt,
                colorspace,
                color_range,
                deint_all = self.deint_all,
                "deinterlacer: (re)criando grafo bwdif"
            );
            self.graph = None;
            let g = FfmpegFilterGraph::new_bwdif(
                Arc::clone(&self.filter_lib),
                Arc::clone(&self.ffmpeg_lib),
                width,
                height,
                pix_fmt,
                colorspace,
                color_range,
                self.deint_all,
            )?;
            self.graph = Some(g);
            self.graph_dims = Some(dims);
        }

        self.graph
            .as_mut()
            .expect("grafo bwdif deve estar inicializado")
            .process(frame)
    }
}
