//! Deinterlacing via bwdif da libavfilter.
//!
//! Ativado quando o stream é detectado como entrelaçado (SPS / field_order /
//! `AV_FRAME_FLAG_INTERLACED`) ou quando `[decoder] deinterlace = force`.
//!
//! # Invariantes (não regredir)
//!
//! 1. **Buffer source** do grafo deve declarar `colorspace` e `range` do frame
//!    de entrada. Sem isso, libavfilter reconfigura o link a cada frame e o
//!    bwdif perde contexto temporal (vídeo congela no primeiro frame).
//! 2. **PTS de saída** deve passar por [`crate::ffi::rescale_bwdif_output_pts`]:
//!    o filtro dobra o tick count (time_base de saída = metade do de entrada).
//!    Sem a divisão por 2, a `VideoQueue` retém todo frame como `TooEarly` vs
//!    o `AudioClock` (90 kHz). Ver L-003 em `.specs/project/STATE.md`.
//!
//! SPEC-AV-005

use std::sync::Arc;

use crate::error::AvError;
use crate::ffi::{
    frame_color_range, frame_colorspace, frame_format, frame_height, frame_width,
    FfmpegFilterGraph, FfmpegFrame, FfmpegLib, FilterLib,
};

/// Deinterlacador baseado em bwdif da libavfilter.
///
/// Criado por PID de vídeo quando o stream é entrelaçado ou forçado via config.
/// Recria o grafo automaticamente se as dimensões do frame mudarem.
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
    /// O grafo é criado lazily na primeira chamada a `process`.
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
    /// Cria o grafo lazily na primeira chamada ou quando as dimensões mudam.
    ///
    /// Retorna `Ok(Some(frame))` com o frame deinterlaced, ou `Ok(None)` se o
    /// filtro bwdif ainda estiver acumulando contexto temporal (AVERROR_EAGAIN).
    ///
    /// SPEC-AV-005
    pub(crate) fn process(&mut self, frame: &FfmpegFrame) -> Result<Option<FfmpegFrame>, AvError> {
        // SAFETY: frame.as_ptr() aponta para um AVFrame válido e preenchido.
        let (width, height, pix_fmt, colorspace, color_range) = unsafe {
            (
                frame_width(frame.as_ptr()) as u32,
                frame_height(frame.as_ptr()) as u32,
                frame_format(frame.as_ptr()),
                frame_colorspace(frame.as_ptr()),
                frame_color_range(frame.as_ptr()),
            )
        };

        // Recria o grafo se as dimensões, formato de pixel, colorspace ou range
        // mudaram. Incluir colorspace/range na chave evita reconfiguração
        // implícita do buffer source a cada frame.
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
