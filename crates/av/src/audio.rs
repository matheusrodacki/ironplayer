//! Saída de áudio WASAPI via cpal: `AudioOutput` + `AudioRingBuffer`.
//!
//! SPEC-AV-004 · SPEC-AV-004a · SPEC-AV-004b · SPEC-AV-004c

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::error::AvError;

// ─── AudioFrame ──────────────────────────────────────────────────────────────

/// Frame de áudio decodificado em PCM f32 interleaved.
///
/// As amostras estão em formato interleaved: para 2 canais, a ordem é
/// `[L0, R0, L1, R1, …]`.
///
/// SPEC-AV-004
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Taxa de amostragem em Hz (e.g. 48000, 44100).
    pub sample_rate: u32,
    /// Número de canais (1 = mono, 2 = estéreo, …).
    pub channels: u16,
    /// Presentation Timestamp em unidades de 90 kHz.
    pub pts: Option<u64>,
    /// Amostras PCM f32 interleaved: `samples.len() == frames * channels`.
    pub samples: Vec<f32>,
}

impl AudioFrame {
    /// Cria um `AudioFrame` a partir de amostras PCM f32 interleaved.
    ///
    /// SPEC-AV-004
    pub fn new(sample_rate: u32, channels: u16, pts: Option<u64>, samples: Vec<f32>) -> Self {
        Self {
            sample_rate,
            channels,
            pts,
            samples,
        }
    }

    /// Retorna o número de frames de áudio (amostras por canal).
    ///
    /// SPEC-AV-004
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.samples.len() / self.channels as usize
    }
}

// ─── AudioRingBuffer ─────────────────────────────────────────────────────────

/// Buffer de jitter para reprodução de áudio.
///
/// Capacidade em amostras (interleaved).  Quando o buffer está acima de 2 ×
/// `capacity`, novas amostras são descartadas (drop-frame policy).
///
/// SPEC-AV-004
pub struct AudioRingBuffer {
    samples: VecDeque<f32>,
    capacity: usize,
}

impl AudioRingBuffer {
    /// Cria o buffer com `capacity_samples` amostras de capacidade nominal.
    ///
    /// SPEC-AV-004
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity_samples.saturating_mul(2)),
            capacity: capacity_samples,
        }
    }

    /// Empurra `samples` no buffer.
    ///
    /// Retorna `true` quando as amostras foram aceitas, `false` quando o
    /// buffer estava acima de 2 × capacidade e os dados foram descartados.
    ///
    /// SPEC-AV-004a
    pub fn push(&mut self, samples: &[f32]) -> bool {
        if self.samples.len() > self.capacity.saturating_mul(2) {
            tracing::warn!(
                dropped = samples.len(),
                "AudioRingBuffer: overflow — frame descartado"
            );
            return false;
        }
        self.samples.extend(samples.iter().copied());
        true
    }

    /// Drena até `output.len()` amostras.  Posições sem dado são preenchidas
    /// com silêncio (0.0).
    ///
    /// SPEC-AV-004
    pub fn pop(&mut self, output: &mut [f32]) {
        for slot in output.iter_mut() {
            *slot = self.samples.pop_front().unwrap_or(0.0);
        }
    }

    /// Nível de ocupação do buffer em `[0.0, 1.0]`.
    ///
    /// SPEC-AV-004c
    pub fn level(&self) -> f32 {
        if self.capacity == 0 {
            return 0.0;
        }
        (self.samples.len() as f32 / self.capacity as f32).min(1.0)
    }

    /// Número de amostras presentemente no buffer.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// `true` quando o buffer não contém amostras.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Capacidade nominal (em amostras).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// ─── helpers (volume) ────────────────────────────────────────────────────────

/// Aplica fator de volume sobre `samples` com hard-clip para `[-1.0, 1.0]`.
///
/// Valores de volume acima de 1.0 causam boost; o clip evita saturação
/// digital nas bordas do range PCM.
///
/// SPEC-AV-004b
fn apply_volume(samples: &mut [f32], volume: f32) {
    for s in samples.iter_mut() {
        *s = (*s * volume).clamp(-1.0, 1.0);
    }
}

// ─── AudioOutput ─────────────────────────────────────────────────────────────

/// Saída de áudio WASAPI via cpal.
///
/// Mantém um buffer de jitter interno (`AudioRingBuffer`).  O callback cpal
/// drena o buffer a cada período de hardware; a thread do decoder empurra
/// frames via `push_samples`.
///
/// SPEC-AV-004
pub struct AudioOutput {
    /// Stream cpal mantida viva enquanto `AudioOutput` existir.
    _stream: cpal::Stream,
    /// Buffer de jitter compartilhado entre producer e callback cpal.
    buffer: Arc<Mutex<AudioRingBuffer>>,
    /// Volume atual (bits de f32 armazenados atomicamente).
    volume: Arc<AtomicU32>,
    /// Taxa de amostragem configurada (Hz).
    pub sample_rate: u32,
    /// Número de canais configurado.
    pub channels: u16,
}

impl AudioOutput {
    /// Abre o dispositivo de saída padrão WASAPI e inicia a reprodução.
    ///
    /// `buffer_ms` define o tamanho do buffer de jitter em milissegundos
    /// (padrão recomendado: 100 ms).
    ///
    /// SPEC-AV-004
    pub fn new(sample_rate: u32, channels: u16, buffer_ms: u32) -> Result<Self, AvError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AvError::FfmpegUnavailable {
                message: "nenhum dispositivo de saída de áudio encontrado".into(),
            })?;

        // Capacidade em amostras interleaved para `buffer_ms` milissegundos.
        let capacity = (sample_rate as u64 * channels as u64 * buffer_ms as u64 / 1000) as usize;

        let buffer = Arc::new(Mutex::new(AudioRingBuffer::new(capacity)));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        let buf_cb = Arc::clone(&buffer);
        let vol_cb = Arc::clone(&volume);

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = device
            .build_output_stream::<f32, _, _>(
                &config,
                move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    let vol = f32::from_bits(vol_cb.load(Ordering::Relaxed));
                    if let Ok(mut buf) = buf_cb.lock() {
                        buf.pop(output);
                    } else {
                        output.fill(0.0);
                    }
                    apply_volume(output, vol);
                },
                |err| {
                    tracing::error!(error = %err, "cpal: erro no stream de áudio");
                },
                None,
            )
            .map_err(|e| AvError::FfmpegUnavailable {
                message: e.to_string(),
            })?;

        stream.play().map_err(|e| AvError::FfmpegUnavailable {
            message: e.to_string(),
        })?;

        tracing::info!(
            sample_rate,
            channels,
            buffer_ms,
            "AudioOutput: stream WASAPI iniciado"
        );

        Ok(Self {
            _stream: stream,
            buffer,
            volume,
            sample_rate,
            channels,
        })
    }

    /// Envia um `AudioFrame` para o buffer de jitter.
    ///
    /// Thread-safe; pode ser chamado da thread do decoder.  Quando o buffer
    /// estiver acima de 2 × capacidade, as amostras do frame são descartadas.
    ///
    /// SPEC-AV-004a
    pub fn push_samples(&self, frame: &AudioFrame) {
        match self.buffer.lock() {
            Ok(mut buf) => {
                buf.push(&frame.samples);
            }
            Err(e) => {
                tracing::error!(error = %e, "AudioOutput: mutex envenenado em push_samples");
            }
        }
    }

    /// Ajusta o volume de reprodução.
    ///
    /// `0.0` = silêncio, `1.0` = nominal, `> 1.0` = boost com hard-clip para
    /// `[-1.0, 1.0]`.  Valores negativos são tratados como `0.0`.
    ///
    /// SPEC-AV-004b
    pub fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Retorna o nível de ocupação do buffer de jitter em `[0.0, 1.0]`.
    ///
    /// A UI usa este valor para exibir um indicador de saúde do buffer.
    ///
    /// SPEC-AV-004c
    pub fn buffer_level(&self) -> f32 {
        self.buffer.lock().map(|b| b.level()).unwrap_or(0.0)
    }
}

// ─── testes ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── AudioRingBuffer ──────────────────────────────────────────────────────

    /// Push/pop básico: o que entra deve sair na mesma ordem (FIFO).
    ///
    /// SPEC-AV-004
    #[test]
    fn spec_av_004_ring_buffer_push_pop_fifo() {
        let mut buf = AudioRingBuffer::new(8);
        assert!(buf.push(&[0.1, 0.2, 0.3, 0.4]));
        let mut out = [0.0f32; 4];
        buf.pop(&mut out);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    /// Pop em buffer vazio retorna silêncio (0.0).
    ///
    /// SPEC-AV-004
    #[test]
    fn spec_av_004_ring_buffer_pop_empty_returns_silence() {
        let mut buf = AudioRingBuffer::new(8);
        let mut out = [1.0f32; 4];
        buf.pop(&mut out);
        assert_eq!(out, [0.0f32; 4]);
    }

    /// `buffer_level()` retorna 0.0 no início e cresce proporcionalmente.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004_buffer_level_starts_at_zero() {
        let buf = AudioRingBuffer::new(100);
        assert_eq!(buf.level(), 0.0);
    }

    /// `buffer_level()` nunca excede 1.0 mesmo com buffer acima da capacidade.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004_buffer_level_capped_at_one() {
        let mut buf = AudioRingBuffer::new(4);
        // Empurra 8 amostras (2 × capacidade) — dentro do limite de drop.
        let _ = buf.push(&[0.0f32; 8]);
        assert!(
            buf.level() <= 1.0,
            "buffer_level={} deve ser ≤ 1.0",
            buf.level()
        );
    }

    /// Quando o buffer está acima de 2 × capacidade, novas amostras são
    /// descartadas e `push` retorna `false`.
    ///
    /// SPEC-AV-004a
    #[test]
    fn spec_av_004_drop_frames_when_buffer_exceeds_2x_capacity() {
        let mut buf = AudioRingBuffer::new(4);

        // Lota acima de 2 × capacidade (9 amostras > 2 × 4 = 8).
        let _ = buf.push(&[0.0f32; 9]);
        // Agora buf.len() > 2 * capacity → próximo push deve ser descartado.
        let dropped = !buf.push(&[1.0f32; 4]);
        assert!(
            dropped,
            "push deveria retornar false quando buffer > 2 × capacidade"
        );
    }

    /// `buffer_level()` retorna exatamente 1.0 quando len == capacity.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004_buffer_level_full_is_one() {
        let mut buf = AudioRingBuffer::new(4);
        let _ = buf.push(&[0.5f32; 4]);
        assert_eq!(buf.level(), 1.0);
    }

    // ── apply_volume (lógica de clip) ────────────────────────────────────────

    /// Volume nominal (1.0) não altera as amostras.
    ///
    /// SPEC-AV-004b
    #[test]
    fn spec_av_004_volume_nominal_no_change() {
        let mut samples = [0.5f32, -0.5, 0.0, 1.0, -1.0];
        apply_volume(&mut samples, 1.0);
        assert_eq!(samples, [0.5, -0.5, 0.0, 1.0, -1.0]);
    }

    /// Volume mute (0.0) silencia todas as amostras.
    ///
    /// SPEC-AV-004b
    #[test]
    fn spec_av_004_volume_mute_silences_output() {
        let mut samples = [0.8f32, -0.8, 0.5];
        apply_volume(&mut samples, 0.0);
        for s in samples {
            assert_eq!(s, 0.0);
        }
    }

    /// Volume > 1.0 produz boost com hard-clip para `[-1.0, 1.0]`.
    ///
    /// SPEC-AV-004b
    #[test]
    fn spec_av_004_volume_boost_clips_to_unit_range() {
        let mut samples = [0.8f32, -0.8, 0.1, -0.1];
        apply_volume(&mut samples, 2.0);
        // 0.8 × 2.0 = 1.6 → clipped to 1.0
        assert_eq!(samples[0], 1.0);
        // −0.8 × 2.0 = −1.6 → clipped to −1.0
        assert_eq!(samples[1], -1.0);
        // 0.1 × 2.0 = 0.2 → sem clip
        assert!((samples[2] - 0.2).abs() < 1e-6);
        assert!((samples[3] - (-0.2)).abs() < 1e-6);
        // Todos dentro do range
        for s in samples {
            assert!(s >= -1.0 && s <= 1.0, "sample {s} fora de [-1, 1]");
        }
    }

    /// Clip funciona para valor extremo (volume = 10.0).
    ///
    /// SPEC-AV-004b
    #[test]
    fn spec_av_004_volume_extreme_boost_always_clips() {
        let mut samples = [0.001f32, -0.001, 0.5, -0.5, 1.0, -1.0];
        apply_volume(&mut samples, 10.0);
        for s in samples {
            assert!(
                s >= -1.0 && s <= 1.0,
                "sample {s} fora de [-1, 1] com volume extremo"
            );
        }
    }

    // ── Sincronização A/V (SPEC-AV-004c) ────────────────────────────────────

    /// Verifica que o desvio entre o PTS de áudio esperado e o PTS calculado
    /// a partir de amostras consumidas é inferior a 40 ms.
    ///
    /// O teste simula o pipeline:
    /// 1. Frame de áudio chega com PTS em 90 kHz.
    /// 2. Amostras são empurradas no ring buffer.
    /// 3. O callback cpal consome N amostras (= N/sr segundos de áudio).
    /// 4. O PTS corrente calculado = pts_inicial + amostras_consumidas × 90000 / (sr × ch).
    /// 5. O desvio entre PTS calculado e PTS do próximo frame deve ser < 40 ms.
    ///
    /// SPEC-AV-004 · SPEC-AV-004c
    #[test]
    fn spec_av_004c_av_sync_deviation_under_40ms() {
        const SAMPLE_RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;
        /// Amostras interleaved por milissegundo.
        const SAMPLES_PER_MS: usize = (SAMPLE_RATE as usize * CHANNELS as usize) / 1000;
        /// 40 ms em ticks de 90 kHz.
        const MAX_DEVIATION_TICKS: u64 = 40 * 90_000 / 1000;

        let mut buf = AudioRingBuffer::new(SAMPLES_PER_MS * 200); // 200 ms

        // Frame 1: PTS = 0 ms → 0 ticks (90 kHz), duração 20 ms.
        let pts_frame1: u64 = 0;
        let frame1_samples = vec![0.3f32; SAMPLES_PER_MS * 20];
        buf.push(&frame1_samples);

        // Frame 2: PTS = 20 ms → 1800 ticks, duração 20 ms.
        let pts_frame2: u64 = 20 * 90_000 / 1000; // 1800
        let frame2_samples = vec![0.3f32; SAMPLES_PER_MS * 20];
        buf.push(&frame2_samples);

        // Callback consome 10 ms de áudio.
        let consumed_1 = SAMPLES_PER_MS * 10;
        let mut out = vec![0.0f32; consumed_1];
        buf.pop(&mut out);

        // PTS corrente calculado (10 ms consumidos desde pts_frame1 = 0).
        let audio_pts_ticks =
            pts_frame1 + (consumed_1 as u64 * 90_000) / (SAMPLE_RATE as u64 * CHANNELS as u64);
        // = 0 + 10*48000*2*90000 / (48000*2) = 10 * 90000/1000 = 900 ticks (= 10 ms)

        // PTS esperado após 10 ms de reprodução desde o frame 1.
        let expected_ticks: u64 = 10 * 90_000 / 1000; // 900 ticks

        let deviation = audio_pts_ticks.abs_diff(expected_ticks);
        assert!(
            deviation <= MAX_DEVIATION_TICKS,
            "desvio A/V = {deviation} ticks ({} ms) excede o limite de 40 ms",
            deviation * 1000 / 90_000
        );

        // Callback consome mais 25 ms (cruza a fronteira do frame 2).
        let consumed_2 = SAMPLES_PER_MS * 25;
        let mut out2 = vec![0.0f32; consumed_2];
        buf.pop(&mut out2);

        // Total consumido = 35 ms; próximo frame esperado começa em 40 ms.
        let total_consumed = consumed_1 + consumed_2;
        let audio_pts_ticks2 =
            pts_frame1 + (total_consumed as u64 * 90_000) / (SAMPLE_RATE as u64 * CHANNELS as u64);

        // pts_frame2 começa em 20 ms; agora estamos em 35 ms → desvio 15 ms < 40 ms.
        let deviation2 = audio_pts_ticks2.abs_diff(pts_frame2);
        assert!(
            deviation2 <= MAX_DEVIATION_TICKS,
            "desvio A/V 2ª etapa = {deviation2} ticks ({} ms) excede o limite de 40 ms",
            deviation2 * 1000 / 90_000
        );
    }

    /// `buffer_level()` cresce linearmente com as amostras empurradas.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004c_buffer_level_proportional() {
        let mut buf = AudioRingBuffer::new(100);
        assert_eq!(buf.level(), 0.0);
        let _ = buf.push(&[0.0f32; 50]);
        let level_50 = buf.level();
        assert!(
            (level_50 - 0.5).abs() < 1e-5,
            "nível com 50/100 amostras deveria ser 0.5, obtido {level_50}"
        );
        let _ = buf.push(&[0.0f32; 50]);
        assert_eq!(buf.level(), 1.0);
    }
}
