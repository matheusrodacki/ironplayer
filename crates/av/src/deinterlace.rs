//! Deinterlacing via bwdif da libavfilter.
//!
//! Ativado automaticamente quando `AVFrame::flags & AV_FRAME_FLAG_INTERLACED != 0`.
//! Cria um grafo avfilter `buffer → bwdif=mode=send_frame:parity=auto:deint=interlaced → buffersink`
//! para cada PID de vídeo na primeira aparição de frame interlaced.
//!
//! O grafo é recriado automaticamente se as dimensões ou o formato de pixel
//! mudarem (raro em transmissão ao vivo, mas suportado para robustez).
//!
//! SPEC-AV-004

use std::sync::Arc;

use crate::error::AvError;
use crate::ffi::{
    frame_format, frame_height, frame_width, FfmpegFilterGraph, FfmpegFrame, FfmpegLib, FilterLib,
};

/// Deinterlacador baseado em bwdif da libavfilter.
///
/// Criado por PID de vídeo quando o primeiro frame interlaced é detectado.
/// Recria o grafo automaticamente se as dimensões do frame mudarem.
///
/// SPEC-AV-004
pub(crate) struct Deinterlacer {
    filter_lib: Arc<FilterLib>,
    ffmpeg_lib: Arc<FfmpegLib>,
    graph: Option<FfmpegFilterGraph>,
    /// Dimensões e formato do grafo atual: `(width, height, pix_fmt)`.
    graph_dims: Option<(u32, u32, i32)>,
}

impl Deinterlacer {
    /// Cria um novo `Deinterlacer` sem grafo ativo.
    ///
    /// O grafo é criado lazily na primeira chamada a `process`.
    ///
    /// SPEC-AV-004
    pub(crate) fn new(filter_lib: Arc<FilterLib>, ffmpeg_lib: Arc<FfmpegLib>) -> Self {
        Self {
            filter_lib,
            ffmpeg_lib,
            graph: None,
            graph_dims: None,
        }
    }

    /// Processa um frame interlaced através do bwdif.
    ///
    /// Cria o grafo lazily na primeira chamada ou quando as dimensões mudam.
    ///
    /// Retorna `Ok(Some(frame))` com o frame deinterlaced, ou `Ok(None)` se o
    /// filtro bwdif ainda estiver acumulando contexto temporal (AVERROR_EAGAIN).
    ///
    /// SPEC-AV-004
    pub(crate) fn process(&mut self, frame: &FfmpegFrame) -> Result<Option<FfmpegFrame>, AvError> {
        // SAFETY: frame.as_ptr() aponta para um AVFrame válido e preenchido.
        let (width, height, pix_fmt) = unsafe {
            (
                frame_width(frame.as_ptr()) as u32,
                frame_height(frame.as_ptr()) as u32,
                frame_format(frame.as_ptr()),
            )
        };

        // Recria o grafo se as dimensões ou o formato de pixel mudaram.
        let dims = (width, height, pix_fmt);
        if self.graph_dims != Some(dims) {
            tracing::debug!(
                width,
                height,
                pix_fmt,
                "deinterlacer: (re)criando grafo bwdif"
            );
            self.graph = None;
            let g = FfmpegFilterGraph::new_bwdif(
                Arc::clone(&self.filter_lib),
                Arc::clone(&self.ffmpeg_lib),
                width,
                height,
                pix_fmt,
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
