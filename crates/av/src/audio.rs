//! Saída de áudio WASAPI via cpal: `AudioOutput` + `AudioRingBuffer`.
//!
//! SPEC-AV-004 · SPEC-AV-004a · SPEC-AV-004b · SPEC-AV-004c

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::clock::{AudioClockHandle, Pts90};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopReport {
    copied_samples: usize,
    missing_samples: usize,
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

    fn hard_limit(&self) -> usize {
        self.capacity.saturating_mul(2)
    }

    fn push_with_drop_oldest(&mut self, samples: &[f32]) -> usize {
        self.samples.extend(samples.iter().copied());

        let hard_limit = self.hard_limit();
        if self.samples.len() <= hard_limit {
            return 0;
        }

        // Mantem a latencia proxima ao jitter buffer configurado descartando
        // audio antigo quando a fila ultrapassa 2x a capacidade nominal.
        let target_len = self.capacity;
        let to_drop = self.samples.len().saturating_sub(target_len);
        for _ in 0..to_drop {
            let _ = self.samples.pop_front();
        }
        to_drop
    }

    /// Empurra `samples` no buffer.
    ///
    /// Retorna `true` quando nao foi necessario descartar amostras antigas.
    ///
    /// SPEC-AV-004a
    pub fn push(&mut self, samples: &[f32]) -> bool {
        self.push_with_drop_oldest(samples) == 0
    }

    /// Drena até `output.len()` amostras.  Posições sem dado são preenchidas
    /// com silêncio (0.0).
    ///
    /// SPEC-AV-004
    pub fn pop(&mut self, output: &mut [f32]) {
        let _ = self.pop_report(output);
    }

    fn pop_report(&mut self, output: &mut [f32]) -> PopReport {
        let mut copied_samples = 0;
        for slot in output.iter_mut() {
            if let Some(sample) = self.samples.pop_front() {
                *slot = sample;
                copied_samples += 1;
            } else {
                *slot = 0.0;
            }
        }

        PopReport {
            copied_samples,
            missing_samples: output.len().saturating_sub(copied_samples),
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

fn sanitize_buffer_ms(buffer_ms: u32) -> u32 {
    match buffer_ms {
        50 | 100 | 200 | 500 => buffer_ms,
        other => {
            tracing::warn!(
                requested_buffer_ms = other,
                "AudioOutput: jitter buffer invalido; usando 100 ms"
            );
            100
        }
    }
}

fn buffer_capacity_samples(sample_rate: u32, channels: u16, buffer_ms: u32) -> usize {
    let samples = sample_rate as u64 * channels as u64 * buffer_ms as u64 / 1000;
    samples.max(channels.max(1) as u64) as usize
}

fn prime_threshold_samples(capacity_samples: usize, channels: u16) -> usize {
    if capacity_samples == 0 {
        return 0;
    }

    capacity_samples
        .saturating_div(2)
        .max(channels.max(1) as usize)
}

struct AudioPlaybackState {
    buffer: AudioRingBuffer,
    primed: bool,
    prime_threshold_samples: usize,
}

impl AudioPlaybackState {
    fn new(capacity_samples: usize, channels: u16) -> Self {
        Self {
            buffer: AudioRingBuffer::new(capacity_samples),
            primed: false,
            prime_threshold_samples: prime_threshold_samples(capacity_samples, channels),
        }
    }

    fn push_samples(&mut self, samples: &[f32]) -> usize {
        self.buffer.push_with_drop_oldest(samples)
    }

    fn pop_for_output(&mut self, output: &mut [f32]) -> PopReport {
        let start_threshold = self
            .prime_threshold_samples
            .max(output.len().min(self.buffer.capacity()));

        if !self.primed && self.buffer.len() < start_threshold {
            output.fill(0.0);
            return PopReport {
                copied_samples: 0,
                missing_samples: output.len(),
            };
        }

        self.primed = true;
        let report = self.buffer.pop_report(output);
        if report.missing_samples > 0 {
            self.primed = false;
        }
        report
    }

    fn buffer_level(&self) -> f32 {
        self.buffer.level()
    }
}

struct AudioSharedState {
    playback: Mutex<AudioPlaybackState>,
    volume: AtomicU32,
    restart_requested: AtomicBool,
    underruns: AtomicU64,
    overruns: AtomicU64,
    /// Contador atômico de samples interleaved consumidos pelo driver WASAPI.
    /// Incrementado pela callback cpal; compartilhado com `AudioClockHandle`.
    ///
    /// SPEC-AV-CLOCK-002
    samples_played: Arc<AtomicU64>,
}

impl AudioSharedState {
    fn new(capacity_samples: usize, channels: u16) -> Self {
        Self {
            playback: Mutex::new(AudioPlaybackState::new(capacity_samples, channels)),
            volume: AtomicU32::new(1.0f32.to_bits()),
            restart_requested: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            samples_played: Arc::new(AtomicU64::new(0)),
        }
    }

    fn request_restart(&self) {
        self.restart_requested.store(true, Ordering::Relaxed);
    }

    fn take_restart_request(&self) {
        self.restart_requested.store(false, Ordering::Relaxed);
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
    stream: Option<cpal::Stream>,
    /// Estado compartilhado entre producer, callback e rotina de recovery.
    shared: Arc<AudioSharedState>,
    /// Taxa de amostragem configurada (Hz).
    pub sample_rate: u32,
    /// Número de canais configurado.
    pub channels: u16,
    buffer_ms: u32,
}

impl AudioOutput {
    fn build_stream(
        sample_rate: u32,
        channels: u16,
        shared: Arc<AudioSharedState>,
    ) -> Result<cpal::Stream, AvError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| AvError::FfmpegUnavailable {
                message: "nenhum dispositivo de saída de áudio encontrado".into(),
            })?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let callback_state = Arc::clone(&shared);
        let error_state = Arc::clone(&shared);
        let stream = device
            .build_output_stream::<f32, _, _>(
                &config,
                move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    let volume = f32::from_bits(callback_state.volume.load(Ordering::Relaxed));

                    let report = match callback_state.playback.try_lock() {
                        Ok(mut playback) => playback.pop_for_output(output),
                        Err(TryLockError::WouldBlock) | Err(TryLockError::Poisoned(_)) => {
                            output.fill(0.0);
                            PopReport {
                                copied_samples: 0,
                                missing_samples: output.len(),
                            }
                        }
                    };

                    if report.missing_samples > 0 {
                        callback_state.underruns.fetch_add(1, Ordering::Relaxed);
                    }

                    // Incrementa o contador de samples consumidos pelo driver.
                    // Usado pelo `AudioClockHandle` para calcular o PTS atual.
                    // SPEC-AV-CLOCK-002
                    callback_state
                        .samples_played
                        .fetch_add(output.len() as u64, Ordering::Relaxed);

                    apply_volume(output, volume);
                },
                move |err| {
                    error_state.request_restart();
                    tracing::warn!(
                        error = %err,
                        sample_rate,
                        channels,
                        "cpal: erro no stream de áudio; recriação agendada"
                    );
                },
                None,
            )
            .map_err(|e| AvError::FfmpegUnavailable {
                message: e.to_string(),
            })?;

        stream.play().map_err(|e| AvError::FfmpegUnavailable {
            message: e.to_string(),
        })?;

        Ok(stream)
    }

    /// Abre o dispositivo de saída padrão WASAPI e inicia a reprodução.
    ///
    /// `buffer_ms` define o tamanho do buffer de jitter em milissegundos
    /// (padrão recomendado: 100 ms).
    ///
    /// SPEC-AV-004
    pub fn new(sample_rate: u32, channels: u16, buffer_ms: u32) -> Result<Self, AvError> {
        let buffer_ms = sanitize_buffer_ms(buffer_ms);
        let capacity = buffer_capacity_samples(sample_rate, channels, buffer_ms);
        let shared = Arc::new(AudioSharedState::new(capacity, channels));
        let stream = Self::build_stream(sample_rate, channels, Arc::clone(&shared))?;

        tracing::info!(
            sample_rate,
            channels,
            buffer_ms,
            "AudioOutput: stream WASAPI iniciado"
        );

        Ok(Self {
            stream: Some(stream),
            shared,
            sample_rate,
            channels,
            buffer_ms,
        })
    }

    /// Envia um `AudioFrame` para o buffer de jitter.
    ///
    /// Thread-safe; pode ser chamado da thread do decoder.  Quando o buffer
    /// estiver acima de 2 × capacidade, as amostras do frame são descartadas.
    ///
    /// SPEC-AV-004a
    pub fn push_samples(&self, frame: &AudioFrame) {
        match self.shared.playback.lock() {
            Ok(mut playback) => {
                let dropped_samples = playback.push_samples(&frame.samples);
                if dropped_samples > 0 {
                    let overruns = self.shared.overruns.fetch_add(1, Ordering::Relaxed) + 1;
                    if overruns == 1 || overruns % 50 == 0 {
                        tracing::warn!(
                            dropped_samples,
                            overruns,
                            sample_rate = self.sample_rate,
                            channels = self.channels,
                            "AudioOutput: overrun no jitter buffer; audio antigo descartado"
                        );
                    }
                }
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
        self.shared
            .volume
            .store(volume.max(0.0).to_bits(), Ordering::Relaxed);
    }

    /// Retorna o nível de ocupação do buffer de jitter em `[0.0, 1.0]`.
    ///
    /// A UI usa este valor para exibir um indicador de saúde do buffer.
    ///
    /// SPEC-AV-004c
    pub fn buffer_level(&self) -> f32 {
        self.shared
            .playback
            .lock()
            .map(|state| state.buffer_level())
            .unwrap_or(0.0)
    }

    /// Retorna um `AudioClockHandle` conectado ao contador de samples desta
    /// saída de áudio.
    ///
    /// O handle compartilha o `Arc<AtomicU64>` interno com a callback cpal —
    /// qualquer chamada a `now_pts90()` no handle refletirá o estado real de
    /// reprodução do driver.
    ///
    /// O `anchor_pts` inicial é `0`; use `AudioClockHandle::reset()` para
    /// reposicionar a âncora após receber o primeiro PTS do stream.
    ///
    /// SPEC-AV-CLOCK-002
    pub fn clock_handle(&self, anchor_pts: Pts90) -> AudioClockHandle {
        AudioClockHandle::with_counter(
            Arc::clone(&self.shared.samples_played),
            self.sample_rate,
            self.channels,
            anchor_pts,
        )
    }

    /// Retorna `true` quando o stream WASAPI sinalizou falha e precisa ser recriado.
    ///
    /// SPEC-AV-004
    pub fn needs_rebuild(&self) -> bool {
        self.shared.restart_requested.load(Ordering::Relaxed)
    }

    /// Tenta recriar o stream de saída usando o dispositivo padrão atual.
    ///
    /// SPEC-AV-004
    pub fn rebuild_stream(&mut self) -> Result<(), AvError> {
        self.stream.take();
        let stream = Self::build_stream(self.sample_rate, self.channels, Arc::clone(&self.shared))?;
        self.stream = Some(stream);
        self.shared.take_restart_request();
        tracing::info!(
            sample_rate = self.sample_rate,
            channels = self.channels,
            buffer_ms = self.buffer_ms,
            "AudioOutput: stream WASAPI recriado"
        );
        Ok(())
    }

    /// Retorna o número de callbacks que precisaram completar o buffer com silêncio.
    ///
    /// SPEC-AV-004c
    pub fn underrun_count(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Retorna o número de vezes em que áudio antigo foi descartado para conter a latência.
    ///
    /// SPEC-AV-004c
    pub fn overrun_count(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
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

        assert!(!buf.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]));
        assert_eq!(buf.len(), 4, "buffer deve voltar para a capacidade nominal");

        let mut out = [0.0f32; 4];
        buf.pop(&mut out);
        assert_eq!(out, [6.0, 7.0, 8.0, 9.0]);
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

    /// O callback so inicia a reproducao quando o jitter buffer tem prefill suficiente.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004c_playback_waits_for_prefill_before_starting() {
        let mut state = AudioPlaybackState::new(8, 2);
        let dropped = state.push_samples(&[0.1, 0.2, 0.3]);
        assert_eq!(dropped, 0);

        let mut out = [1.0f32; 4];
        let report = state.pop_for_output(&mut out);
        assert_eq!(report.copied_samples, 0);
        assert_eq!(report.missing_samples, 4);
        assert_eq!(out, [0.0; 4]);

        let dropped = state.push_samples(&[0.4, 0.5, 0.6, 0.7, 0.8]);
        assert_eq!(dropped, 0);

        let report = state.pop_for_output(&mut out);
        assert_eq!(report.copied_samples, 4);
        assert_eq!(report.missing_samples, 0);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    /// Underrun depois de iniciado rebaixa o estado para re-prime e conta silencio.
    ///
    /// SPEC-AV-004c
    #[test]
    fn spec_av_004c_underrun_resets_primed_state() {
        let mut state = AudioPlaybackState::new(8, 2);
        let _ = state.push_samples(&[0.1, 0.2, 0.3, 0.4]);

        let mut out = [0.0f32; 4];
        let started = state.pop_for_output(&mut out);
        assert_eq!(started.missing_samples, 0);
        assert!(state.primed);

        let mut out2 = [0.0f32; 4];
        let underrun = state.pop_for_output(&mut out2);
        assert_eq!(underrun.copied_samples, 0);
        assert_eq!(underrun.missing_samples, 4);
        assert!(!state.primed);
    }

    /// Apenas 50, 100, 200 e 500 ms sao aceitos como jitter buffer configuravel.
    ///
    /// SPEC-AV-004
    #[test]
    fn spec_av_004_supported_buffer_sizes_are_enforced() {
        assert_eq!(sanitize_buffer_ms(50), 50);
        assert_eq!(sanitize_buffer_ms(100), 100);
        assert_eq!(sanitize_buffer_ms(200), 200);
        assert_eq!(sanitize_buffer_ms(500), 500);
        assert_eq!(sanitize_buffer_ms(75), 100);
    }
}
