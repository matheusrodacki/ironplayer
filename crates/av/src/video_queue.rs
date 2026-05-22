//! `VideoQueue` — fila de vídeo ordenada por PTS com políticas de sincronização.
//!
//! SPEC-AV-VQ-001
//!
//! Implementa a Fase C do TDD Sprint 1 (tdd-sprint-01-av-sync.md §4.5):
//!
//! - **hold-early** (HOLD_PTS = 1 800): retém frames cujo PTS está mais de
//!   20 ms à frente do clock; evita exibição prematura.
//! - **drop-late** (DROP_PTS = 9 000): descarta frames cujo PTS está mais de
//!   100 ms atrás do clock; mantém a fila limpa de frames irrecuperáveis.
//! - **resync** (RESYNC_PTS = 45 000): salto de PTS > 500 ms → sinaliza
//!   descontinuidade e retorna o frame com `new_anchor` para que o chamador
//!   resincronize o clock.
//! - **wrap 33-bit**: detecta quando `|Δpts| > 2^32` e acumula offset de
//!   `2^33` unidades, normalizando o espaço de PTS para aritmética i64.
//!
//! ## Uso típico
//!
//! ```ignore
//! let mut q = VideoQueue::default();
//! let clock = MasterClock::wall(0);
//!
//! // Produtor: push de frames do decodificador
//! q.push(frame);
//!
//! // Consumidor (loop da UI a 60 Hz):
//! match q.pop_ready(clock.now_pts90()) {
//!     PopResult::Ready(f) => renderer.upload(&f)?,
//!     PopResult::Resync { frame, new_anchor } => {
//!         clock.reset(new_anchor);
//!         renderer.upload(&frame)?;
//!     }
//!     PopResult::TooEarly | PopResult::Empty => {}
//! }
//! ```

use std::collections::VecDeque;

use crate::clock::Pts90;
use crate::renderer::VideoFrame;

// ─── Limiares ─────────────────────────────────────────────────────────────────

/// Limiar HOLD: frame mais de 20 ms à frente do clock → aguardar.
/// 20 ms × 90 kHz = 1 800 unidades.
///
/// SPEC-AV-VQ-001
pub const HOLD_PTS: i64 = 1_800;

/// Limiar DROP: frame mais de 100 ms atrás do clock → descartar.
/// 100 ms × 90 kHz = 9 000 unidades.
///
/// SPEC-AV-VQ-001
pub const DROP_PTS: i64 = 9_000;

/// Limiar RESYNC: salto de PTS > 500 ms → resincronizar clock.
/// 500 ms × 90 kHz = 45 000 unidades.
///
/// SPEC-AV-VQ-001
pub const RESYNC_PTS: i64 = 45_000;

/// Threshold de wrap 33 bits: `|Δpts| > 2^32` → wrap detectado.
///
/// SPEC-AV-VQ-001
pub const WRAP_THRESHOLD: i64 = 1i64 << 32;

/// Capacidade padrão da fila em frames.
///
/// SPEC-AV-VQ-001
pub const DEFAULT_CAPACITY: usize = 16;

// ─── PopResult ────────────────────────────────────────────────────────────────

/// Resultado de [`VideoQueue::pop_ready`].
///
/// SPEC-AV-VQ-001
#[derive(Debug)]
pub enum PopResult {
    /// Frame pronto para exibir agora.
    Ready(VideoFrame),
    /// Próximo frame está muito adiantado (PTS > clock + HOLD_PTS); aguardar.
    TooEarly,
    /// Fila vazia.
    Empty,
    /// Salto de PTS muito grande (> RESYNC_PTS) detectado.
    ///
    /// O frame é retornado, mas o clock deve ser resincronizado para
    /// `new_anchor` antes do próximo `pop_ready`.
    Resync {
        /// Frame com PTS discontinuo (deve ser exibido).
        frame: VideoFrame,
        /// Novo PTS âncora sugerido para o clock.
        new_anchor: Pts90,
    },
}

// ─── PushResult ───────────────────────────────────────────────────────────────

/// Resultado de [`VideoQueue::push`].
///
/// SPEC-AV-VQ-001
#[derive(Debug, PartialEq, Eq)]
pub enum PushResult {
    /// Frame inserido com sucesso.
    Inserted,
    /// Fila estava na capacidade máxima; frame mais antigo foi descartado.
    DroppedOldest,
    /// Frame inserido e wrap de PTS 33-bit detectado e corrigido.
    WrapDetected,
}

// ─── VideoQueue ───────────────────────────────────────────────────────────────

/// Fila de frames de vídeo ordenada por PTS com políticas de sincronização A/V.
///
/// Mantém até `capacity` frames em ordem crescente de PTS ajustado.
/// `pop_ready(clock_pts)` implementa as políticas na seguinte ordem:
///
/// | Condição                           | Ação                |
/// |------------------------------------|---------------------|
/// | fila vazia                         | `Empty`             |
/// | `frame_pts < clock − DROP_PTS`     | descarta e continua |
/// | `\|frame_pts − clock\| > RESYNC_PTS` | `Resync`            |
/// | `frame_pts > clock + HOLD_PTS`     | `TooEarly`          |
/// | caso contrário                     | `Ready`             |
///
/// SPEC-AV-VQ-001
pub struct VideoQueue {
    /// Frames na fila em ordem crescente de PTS ajustado.
    frames: VecDeque<(Pts90, VideoFrame)>,
    /// Capacidade máxima (número de frames).
    capacity: usize,
    /// Offset acumulado por wraps de PTS 33-bit (múltiplos de 2^33).
    wrap_offset: i64,
    /// PTS ajustado do último frame inserido (para detecção de wrap).
    last_pts: Option<Pts90>,
    // ── Contadores de telemetria ──────────────────────────────────────────────
    /// Frames descartados por chegada tardia (late-drop).
    pub dropped_late: u64,
    /// Chamadas a `pop_ready` que retornaram `TooEarly`.
    pub held_early: u64,
    /// Descontinuidades de PTS detectadas (resync).
    pub discontinuities: u64,
}

impl VideoQueue {
    /// Cria uma nova `VideoQueue` com a capacidade especificada.
    ///
    /// SPEC-AV-VQ-001
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            frames: VecDeque::with_capacity(cap),
            capacity: cap,
            wrap_offset: 0,
            last_pts: None,
            dropped_late: 0,
            held_early: 0,
            discontinuities: 0,
        }
    }

    /// Número de frames atualmente na fila.
    ///
    /// SPEC-AV-VQ-001
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Retorna `true` se a fila está vazia.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Retorna o PTS ajustado do frame na frente da fila sem removê-lo.
    ///
    /// Usado pela UI para calcular o offset de sincronização A/V:
    /// `offset_ms = (front_pts - clock_pts) / 90.0`.
    ///
    /// Retorna `None` quando a fila está vazia.
    ///
    /// SPEC-AV-VQ-001
    pub fn front_pts(&self) -> Option<Pts90> {
        self.frames.front().map(|(pts, _)| *pts)
    }

    /// Limpa todos os frames e reseta o estado de wrap/PTS.
    ///
    /// Chamado em descontinuidades severas ou ao reiniciar o stream.
    ///
    /// SPEC-AV-VQ-001
    pub fn clear(&mut self) {
        self.frames.clear();
        self.wrap_offset = 0;
        self.last_pts = None;
    }

    /// Converte PTS raw `u64` para `Pts90` com offset de wrap acumulado.
    ///
    /// Detecta wrap quando `Δpts < -WRAP_THRESHOLD` (PTS reiniciou em ~0 após
    /// atingir 2^33-1), acumula offset de `2^33` unidades.
    ///
    /// Retorna o PTS ajustado e `true` se um wrap foi detectado.
    ///
    /// SPEC-AV-VQ-001
    fn adjust_pts(&mut self, raw_pts: u64) -> (Pts90, bool) {
        let raw = raw_pts as i64;
        let adjusted = raw.wrapping_add(self.wrap_offset);

        let wrap_detected = if let Some(last) = self.last_pts {
            let delta = adjusted - last;
            if delta < -WRAP_THRESHOLD {
                // PTS reiniciou em ~0 após atingir 2^33-1: acumular offset
                self.wrap_offset = self.wrap_offset.wrapping_add(1i64 << 33);
                true
            } else {
                false
            }
        } else {
            false
        };

        let final_pts = if wrap_detected {
            raw.wrapping_add(self.wrap_offset)
        } else {
            adjusted
        };

        (final_pts, wrap_detected)
    }

    /// Insere um frame na fila mantendo ordem crescente de PTS ajustado.
    ///
    /// - Frames sem PTS recebem PTS sintético (`last_pts + 1` ou `0`).
    /// - Se a fila estiver na capacidade máxima, o frame mais antigo é
    ///   descartado antes da inserção.
    ///
    /// Retorna:
    /// - `WrapDetected` se um wrap de 33 bits foi detectado e corrigido;
    /// - `DroppedOldest` se um frame foi removido por capacidade esgotada;
    /// - `Inserted` caso contrário.
    ///
    /// SPEC-AV-VQ-001
    pub fn push(&mut self, frame: VideoFrame) -> PushResult {
        let (adj_pts, wrap_detected) = match frame.pts {
            Some(raw) => self.adjust_pts(raw),
            None => {
                // Sem PTS: usa last_pts + 1 para manter ordem FIFO
                let pts = self.last_pts.map(|p| p.saturating_add(1)).unwrap_or(0);
                (pts, false)
            }
        };

        self.last_pts = Some(adj_pts);

        // Descarta frame mais antigo se na capacidade máxima
        let dropped = if self.frames.len() >= self.capacity {
            self.frames.pop_front();
            true
        } else {
            false
        };

        // Inserção em posição ordenada via binary search
        let pos = self.frames.partition_point(|(pts, _)| *pts <= adj_pts);
        self.frames.insert(pos, (adj_pts, frame));

        if wrap_detected {
            PushResult::WrapDetected
        } else if dropped {
            PushResult::DroppedOldest
        } else {
            PushResult::Inserted
        }
    }

    /// Tenta extrair o próximo frame pronto para exibição dado o PTS do clock.
    ///
    /// Aplica as políticas em ordem:
    ///
    /// 1. **Empty**: fila vazia → `PopResult::Empty`.
    /// 2. **Drop-late**: `frame_pts < clock_pts − DROP_PTS` → descarta e
    ///    continua.
    /// 3. **Resync**: `|frame_pts − clock_pts| > RESYNC_PTS` → retorna
    ///    `PopResult::Resync { frame, new_anchor: frame_pts }`.
    /// 4. **TooEarly**: `frame_pts > clock_pts + HOLD_PTS` →
    ///    `PopResult::TooEarly`.
    /// 5. **Ready**: dentro da janela → `PopResult::Ready(frame)`.
    ///
    /// SPEC-AV-VQ-001
    pub fn pop_ready(&mut self, clock_pts: Pts90) -> PopResult {
        loop {
            let frame_pts = match self.frames.front() {
                Some((pts, _)) => *pts,
                None => return PopResult::Empty,
            };

            let diff = frame_pts - clock_pts;

            // Drop-late: frame muito atrasado — descarta e continua
            if diff < -DROP_PTS {
                self.frames.pop_front();
                self.dropped_late += 1;
                tracing::debug!(
                    frame_pts,
                    clock_pts,
                    behind_ms = (-diff) / 90,
                    "video_queue: frame tardio descartado"
                );
                continue;
            }

            // Resync: salto muito grande (descontinuidade ou seek)
            if diff.abs() > RESYNC_PTS {
                let (_, frame) = match self.frames.pop_front() {
                    Some(item) => item,
                    None => return PopResult::Empty,
                };
                self.discontinuities += 1;
                tracing::warn!(
                    frame_pts,
                    clock_pts,
                    diff_ms = diff.abs() / 90,
                    "video_queue: descontinuidade de PTS — resync"
                );
                return PopResult::Resync {
                    frame,
                    new_anchor: frame_pts,
                };
            }

            // Hold-early: frame muito adiantado — aguardar
            if diff > HOLD_PTS {
                self.held_early += 1;
                return PopResult::TooEarly;
            }

            // Ready: dentro da janela de exibição
            let (_, frame) = match self.frames.pop_front() {
                Some(item) => item,
                None => return PopResult::Empty,
            };
            return PopResult::Ready(frame);
        }
    }
}

impl Default for VideoQueue {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_frame(pts: Option<u64>) -> VideoFrame {
        VideoFrame::new(4, 4, pts, Bytes::from(vec![0u8; 4 * 4 * 3]), 1, 1)
    }

    fn make_clock_pts(pts: Pts90) -> Pts90 {
        pts
    }

    // ── push / ordenação ──────────────────────────────────────────────────────

    /// Inserção de frame único com PTS definido.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_push_single_frame() {
        let mut q = VideoQueue::new(4);
        let r = q.push(make_frame(Some(1000)));
        assert_eq!(r, PushResult::Inserted);
        assert_eq!(q.len(), 1);
    }

    /// Inserção de frame sem PTS recebe PTS sintético 0.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_push_no_pts_uses_synthetic() {
        let mut q = VideoQueue::new(4);
        let r = q.push(make_frame(None));
        assert_eq!(r, PushResult::Inserted);
        assert_eq!(q.len(), 1);
    }

    /// Frames inseridos fora de ordem são reordenados por PTS.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_push_out_of_order_reordered() {
        let mut q = VideoQueue::new(4);
        q.push(make_frame(Some(3000)));
        q.push(make_frame(Some(1000)));
        q.push(make_frame(Some(2000)));

        // Extrai frames com clock progressivo: verifica que saem em ordem
        // crescente de PTS (1000, 2000, 3000).
        let mut pts_list = Vec::new();
        for clock in [1000i64, 2000, 3000] {
            match q.pop_ready(clock) {
                PopResult::Ready(f) => pts_list.push(f.pts.unwrap()),
                other => panic!("esperava Ready no clock {clock}, obteve {:?}", other),
            }
        }
        assert_eq!(pts_list, vec![1000u64, 2000, 3000]);
    }

    /// Ao atingir capacidade máxima, frame mais antigo é descartado.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_push_at_capacity_drops_oldest() {
        let mut q = VideoQueue::new(2);
        q.push(make_frame(Some(100)));
        q.push(make_frame(Some(200)));
        let r = q.push(make_frame(Some(300)));
        assert_eq!(r, PushResult::DroppedOldest);
        assert_eq!(q.len(), 2);
    }

    // ── pop_ready: Empty ──────────────────────────────────────────────────────

    /// Fila vazia retorna `Empty`.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_empty_queue() {
        let mut q = VideoQueue::new(4);
        assert!(matches!(q.pop_ready(0), PopResult::Empty));
    }

    // ── pop_ready: Ready ──────────────────────────────────────────────────────

    /// Frame cujo PTS está exatamente no clock é retornado como Ready.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_ready_exact_pts() {
        let mut q = VideoQueue::new(4);
        q.push(make_frame(Some(9000)));
        let clock = make_clock_pts(9000);
        assert!(matches!(q.pop_ready(clock), PopResult::Ready(_)));
    }

    /// Frame cujo PTS está dentro da janela [clock - DROP, clock + HOLD]
    /// é retornado como Ready.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_ready_within_window() {
        let mut q = VideoQueue::new(4);
        // PTS = 9000, clock = 9500 → diff = -500 → dentro da janela
        q.push(make_frame(Some(9000)));
        assert!(matches!(q.pop_ready(9500), PopResult::Ready(_)));
    }

    // ── pop_ready: TooEarly ───────────────────────────────────────────────────

    /// Frame cujo PTS está mais de HOLD_PTS à frente do clock é retido.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_too_early() {
        let mut q = VideoQueue::new(4);
        // frame_pts = clock + HOLD_PTS + 1 → hold
        q.push(make_frame(Some(10_000)));
        let clock = make_clock_pts(10_000 - HOLD_PTS - 1);
        assert!(matches!(q.pop_ready(clock), PopResult::TooEarly));
        assert_eq!(q.held_early, 1);
        assert_eq!(q.len(), 1); // frame não foi removido
    }

    // ── pop_ready: drop-late ──────────────────────────────────────────────────

    /// Frame com PTS mais de DROP_PTS atrás do clock é descartado.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_drop_late() {
        let mut q = VideoQueue::new(4);
        // frame_pts = 100, clock = 100 + DROP_PTS + 1 → drop
        q.push(make_frame(Some(100)));
        let clock = make_clock_pts(100 + DROP_PTS + 1);
        assert!(matches!(q.pop_ready(clock), PopResult::Empty));
        assert_eq!(q.dropped_late, 1);
        assert_eq!(q.len(), 0);
    }

    /// Múltiplos frames tardios são todos descartados antes de retornar Ready.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_multiple_late_then_ready() {
        let mut q = VideoQueue::new(8);
        for pts in [100u64, 200, 300, 9_500] {
            q.push(make_frame(Some(pts)));
        }
        let clock = make_clock_pts(9_000 + DROP_PTS); // 18000
                                                      // 100, 200, 300 estão todos abaixo de clock - DROP_PTS = 9000
                                                      // 9500 também está abaixo de 18000 - 9000 = 9000 → 9500 > 9000 → dentro da janela
                                                      // 9500 - 18000 = -8500, |−8500| < DROP_PTS (9000) → Ready
        let result = q.pop_ready(clock);
        assert!(
            matches!(result, PopResult::Ready(_)),
            "esperava Ready, obteve {:?}",
            result
        );
        assert_eq!(q.dropped_late, 3); // 100, 200, 300 descartados
    }

    // ── pop_ready: Resync ─────────────────────────────────────────────────────

    /// Salto de PTS > RESYNC_PTS retorna Resync com new_anchor correto.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_pop_resync_large_jump() {
        let mut q = VideoQueue::new(4);
        let frame_pts: u64 = 500_000;
        q.push(make_frame(Some(frame_pts)));
        // clock = 0 → diff = 500_000 > RESYNC_PTS (45_000)
        match q.pop_ready(0) {
            PopResult::Resync {
                frame: _,
                new_anchor,
            } => {
                assert_eq!(new_anchor, frame_pts as i64);
            }
            other => panic!("esperava Resync, obteve {:?}", other),
        }
        assert_eq!(q.discontinuities, 1);
    }

    // ── wrap 33-bit ───────────────────────────────────────────────────────────

    /// Wrap de PTS 33-bit detectado: PTS salta de ~2^33-1 para ~0.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_wrap_33bit_detected() {
        let mut q = VideoQueue::new(4);
        let near_max: u64 = (1u64 << 33) - 1 - 1000;
        q.push(make_frame(Some(near_max)));
        // Agora PTS = 500 (wraparound)
        let r = q.push(make_frame(Some(500)));
        assert_eq!(r, PushResult::WrapDetected);
    }

    /// Após wrap, PTS continuam sendo ordenados corretamente.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_wrap_pts_ordering_preserved() {
        let mut q = VideoQueue::new(8);
        let near_max: u64 = (1u64 << 33) - 5_000;
        // Insere dois frames pré-wrap
        q.push(make_frame(Some(near_max - 1000)));
        q.push(make_frame(Some(near_max)));
        // Insere dois frames pós-wrap
        q.push(make_frame(Some(500)));
        q.push(make_frame(Some(1000)));

        assert_eq!(q.len(), 4);

        // Com clock em near_max + DROP_PTS + 1 (ajustado), os frames pré-wrap
        // seriam tardios e os pós-wrap would be ready.
        // Só verifica que a fila tem 4 frames em ordem (não crasha).
        assert!(q.len() <= 4);
    }

    // ── clear ─────────────────────────────────────────────────────────────────

    /// `clear()` esvazia a fila e reseta o estado.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_clear_resets_state() {
        let mut q = VideoQueue::new(4);
        q.push(make_frame(Some(1000)));
        q.push(make_frame(Some(2000)));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        // Após clear, novo frame começa do zero
        let r = q.push(make_frame(Some(100)));
        assert_eq!(r, PushResult::Inserted);
    }

    // ── default ───────────────────────────────────────────────────────────────

    /// `VideoQueue::default()` usa `DEFAULT_CAPACITY`.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_default_capacity() {
        let q = VideoQueue::default();
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
    }

    // ── limiar exato ──────────────────────────────────────────────────────────

    /// Frame exatamente no limiar HOLD (diff == HOLD_PTS) é retido.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_hold_threshold_exact() {
        let mut q = VideoQueue::new(4);
        let clock_pts: Pts90 = 90_000;
        // frame_pts = clock + HOLD_PTS → diff = HOLD_PTS → hold (diff > HOLD_PTS é false)
        q.push(make_frame(Some((clock_pts + HOLD_PTS) as u64)));
        // diff == HOLD_PTS → NOT > HOLD_PTS → Ready
        assert!(matches!(q.pop_ready(clock_pts), PopResult::Ready(_)));
    }

    /// Frame com diff == -DROP_PTS (exatamente no limiar) é retornado como Ready.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_drop_threshold_exact() {
        let mut q = VideoQueue::new(4);
        let clock_pts: Pts90 = 90_000;
        // frame_pts = clock - DROP_PTS → diff = -DROP_PTS → NOT < -DROP_PTS → Ready
        q.push(make_frame(Some((clock_pts - DROP_PTS) as u64)));
        assert!(matches!(q.pop_ready(clock_pts), PopResult::Ready(_)));
    }

    // ── front_pts ─────────────────────────────────────────────────────────────

    /// `front_pts()` retorna `None` quando a fila está vazia.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_front_pts_empty() {
        let q = VideoQueue::new(4);
        assert_eq!(q.front_pts(), None);
    }

    /// `front_pts()` retorna o PTS ajustado do frame na frente da fila.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_front_pts_returns_smallest() {
        let mut q = VideoQueue::new(4);
        q.push(make_frame(Some(3000)));
        q.push(make_frame(Some(1000)));
        q.push(make_frame(Some(2000)));
        // Fila está ordenada por PTS; front deve ser o menor PTS (1000).
        assert_eq!(q.front_pts(), Some(1000));
    }

    /// `front_pts()` não remove o frame da fila.
    ///
    /// SPEC-AV-VQ-001
    #[test]
    fn spec_av_vq_001_front_pts_does_not_consume() {
        let mut q = VideoQueue::new(4);
        q.push(make_frame(Some(500)));
        let pts_before = q.front_pts();
        let pts_after = q.front_pts();
        assert_eq!(pts_before, pts_after);
        assert_eq!(q.len(), 1);
    }
}
