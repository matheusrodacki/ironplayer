//! Master clock para sincronização A/V.
//!
//! SPEC-AV-CLOCK-001 · SPEC-AV-CLOCK-002 · SPEC-AV-CLOCK-003
//!
//! # Visão geral
//!
//! Implementa as Fases A e B do TDD Sprint 1 (tdd-sprint-01-av-sync.md §4.5):
//!
//! - **Fase A**: tipo `Pts90` e trait `Clock` com contrato de `now_pts90()` /
//!   `reset()` — base para instrumentação de drift.
//! - **Fase B**: `AudioClockHandle` (contador atômico de samples consumidos
//!   pela callback cpal) e `WallClockHandle` (fallback wall-clock) expostos
//!   via `MasterClock`.
//!
//! ## Unidade interna
//!
//! `Pts90 = i64` em 90 kHz, compatível com FFmpeg.  Wrap de 33 bits
//! detectado em `VideoQueue` (Fase C) quando `Δpts > 2^32`.
//!
//! ## Fórmula de `AudioClockHandle::now_pts90`
//!
//! ```text
//! now_pts90 = anchor_pts + (samples_played / channels / sample_rate) * 90_000
//! ```
//!
//! Onde `samples_played` é um `AtomicU64` incrementado pela callback cpal em
//! `output.len()` a cada chamada.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ─── Pts90 ────────────────────────────────────────────────────────────────────

/// Timestamp de apresentação em unidades de 90 kHz (`i64` com offset de wrap).
///
/// Compatível com a representação interna do FFmpeg (`AVFrame::pts` em
/// `AV_TIME_BASE = 90 000`).  Valores negativos são válidos (pré-âncora).
///
/// SPEC-AV-CLOCK-001
pub type Pts90 = i64;

// ─── Clock trait ──────────────────────────────────────────────────────────────

/// Abstração de relógio para sincronização A/V.
///
/// Implementada por [`AudioClockHandle`] (default quando há áudio) e
/// [`WallClockHandle`] (fallback sem áudio).
///
/// SPEC-AV-CLOCK-001
pub trait Clock: Send + Sync {
    /// Retorna o PTS estimado do momento presente em 90 kHz.
    ///
    /// SPEC-AV-CLOCK-001
    fn now_pts90(&self) -> Pts90;

    /// Reposiciona o relógio: define `anchor_pts` como novo ponto de origem.
    ///
    /// Chamado em `PcrEvent::Discontinuity` ou quando o stream é reaberto.
    ///
    /// SPEC-AV-CLOCK-001
    fn reset(&self, anchor_pts: Pts90);
}

// ─── AudioClockHandle ────────────────────────────────────────────────────────

/// Handle do relógio baseado em samples de áudio consumidos pelo WASAPI.
///
/// O campo `samples_played` é um `Arc<AtomicU64>` compartilhado com a
/// callback cpal: a callback incrementa o contador com `output.len()` a
/// cada invocação; `now_pts90()` lê o contador e converte para PTS 90 kHz.
///
/// O relógio mede *samples interleaved* (i.e. `channels` amostras por frame
/// de áudio), portanto a conversão para tempo real é:
///
/// ```text
/// elapsed_s = samples_played / channels / sample_rate
/// now_pts90 = anchor_pts + elapsed_s * 90_000
/// ```
///
/// SPEC-AV-CLOCK-002
#[derive(Clone, Debug)]
pub struct AudioClockHandle {
    /// Contador atômico de samples interleaved consumidos pelo driver de
    /// áudio.  Compartilhado com a callback cpal via `Arc`.
    pub samples_played: Arc<AtomicU64>,
    /// Taxa de amostragem em Hz (e.g. 48 000).
    pub sample_rate: u32,
    /// Número de canais (1 = mono, 2 = estéreo).
    pub channels: u16,
    /// PTS de âncora em 90 kHz.  Pode ser atualizado atomicamente via
    /// `reset()`.
    anchor_pts: Arc<AtomicI64>,
}

impl AudioClockHandle {
    /// Cria um novo `AudioClockHandle` com âncora inicial em `anchor_pts`.
    ///
    /// SPEC-AV-CLOCK-002
    pub fn new(sample_rate: u32, channels: u16, anchor_pts: Pts90) -> Self {
        Self {
            samples_played: Arc::new(AtomicU64::new(0)),
            sample_rate,
            channels,
            anchor_pts: Arc::new(AtomicI64::new(anchor_pts)),
        }
    }

    /// Cria um `AudioClockHandle` usando um `Arc<AtomicU64>` já existente como
    /// contador de samples.
    ///
    /// Usado por `AudioOutput::clock_handle()` para compartilhar o mesmo
    /// contador atômico da callback cpal.
    ///
    /// SPEC-AV-CLOCK-002
    pub fn with_counter(
        samples_played: Arc<AtomicU64>,
        sample_rate: u32,
        channels: u16,
        anchor_pts: Pts90,
    ) -> Self {
        Self {
            samples_played,
            sample_rate,
            channels,
            anchor_pts: Arc::new(AtomicI64::new(anchor_pts)),
        }
    }

    /// Retorna um clone do `Arc<AtomicU64>` para ser usado na callback cpal.
    ///
    /// A callback deve chamar:
    /// ```rust,ignore
    /// samples_played.fetch_add(output.len() as u64, Ordering::Relaxed);
    /// ```
    ///
    /// SPEC-AV-CLOCK-002
    pub fn samples_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.samples_played)
    }
}

impl Clock for AudioClockHandle {
    /// Calcula o PTS presente a partir dos samples consumidos.
    ///
    /// ```text
    /// elapsed_s  = samples_played / channels / sample_rate
    /// now_pts90  = anchor_pts + elapsed_s * 90_000
    /// ```
    ///
    /// Usa `Ordering::Relaxed` no contador de samples — suficiente pois
    /// `now_pts90` é lido em frequência ≤ 60 Hz pela UI, sem sincronização
    /// crítica com o produtor de samples.
    ///
    /// SPEC-AV-CLOCK-002
    fn now_pts90(&self) -> Pts90 {
        let played = self.samples_played.load(Ordering::Relaxed);
        let anchor = self.anchor_pts.load(Ordering::Relaxed);

        if self.channels == 0 || self.sample_rate == 0 {
            return anchor;
        }

        // samples interleaved → frames de áudio → segundos → 90 kHz
        let frames = played / self.channels as u64;
        // Usar aritmética inteira para evitar perda de precisão em f64 com
        // valores grandes de `played`.
        // frames * 90_000 / sample_rate
        let pts_offset = frames
            .saturating_mul(90_000)
            .saturating_div(self.sample_rate as u64) as i64;

        anchor.saturating_add(pts_offset)
    }

    /// Reposiciona o relógio zerando `samples_played` e definindo nova âncora.
    ///
    /// SPEC-AV-CLOCK-002
    fn reset(&self, anchor_pts: Pts90) {
        self.samples_played.store(0, Ordering::Relaxed);
        self.anchor_pts.store(anchor_pts, Ordering::Relaxed);
    }
}

// ─── WallClockHandle ─────────────────────────────────────────────────────────

/// Relógio de fallback baseado em `Instant` (wall clock).
///
/// Usado quando não há áudio ou antes da primeira amostra ser consumida.
/// A conversão para PTS 90 kHz usa a âncora definida em `reset()`.
///
/// SPEC-AV-CLOCK-003
#[derive(Debug)]
pub struct WallClockHandle {
    /// Instante de referência (âncora wall clock).
    origin: std::sync::Mutex<Instant>,
    /// PTS de âncora em 90 kHz correspondente a `origin`.
    anchor_pts: AtomicI64,
}

impl WallClockHandle {
    /// Cria um novo `WallClockHandle` com âncora em `anchor_pts`.
    ///
    /// O instante de origem é capturado em `Instant::now()` no momento da
    /// criação.
    ///
    /// SPEC-AV-CLOCK-003
    pub fn new(anchor_pts: Pts90) -> Self {
        Self {
            origin: std::sync::Mutex::new(Instant::now()),
            anchor_pts: AtomicI64::new(anchor_pts),
        }
    }
}

impl Clock for WallClockHandle {
    /// Calcula o PTS presente a partir do tempo decorrido desde a âncora.
    ///
    /// ```text
    /// elapsed_s = (Instant::now() - origin).as_secs_f64()
    /// now_pts90 = anchor_pts + (elapsed_s * 90_000) as i64
    /// ```
    ///
    /// SPEC-AV-CLOCK-003
    fn now_pts90(&self) -> Pts90 {
        let origin = self
            .origin
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Instant::now());
        let elapsed_s = origin.elapsed().as_secs_f64();
        let pts_offset = (elapsed_s * 90_000.0) as i64;
        let anchor = self.anchor_pts.load(Ordering::Relaxed);
        anchor.saturating_add(pts_offset)
    }

    /// Reposiciona o relógio: captura novo `Instant::now()` e define nova âncora.
    ///
    /// SPEC-AV-CLOCK-003
    fn reset(&self, anchor_pts: Pts90) {
        if let Ok(mut origin) = self.origin.lock() {
            *origin = Instant::now();
        }
        self.anchor_pts.store(anchor_pts, Ordering::Relaxed);
    }
}

// ─── MasterClock ─────────────────────────────────────────────────────────────

/// Relógio master selecionável para o pipeline A/V.
///
/// Seleciona automaticamente entre `AudioClock` (padrão quando há áudio) e
/// `Wall` (fallback).  O variant `Audio` é o único com controle de samples
/// atômicos da callback cpal.
///
/// SPEC-AV-CLOCK-001
pub enum MasterClock {
    /// Relógio baseado em samples de áudio consumidos pelo driver WASAPI.
    Audio(AudioClockHandle),
    /// Relógio de fallback baseado em wall clock (`Instant`).
    Wall(WallClockHandle),
}

impl MasterClock {
    /// Cria um `MasterClock::Audio` com os parâmetros fornecidos.
    ///
    /// SPEC-AV-CLOCK-001
    pub fn audio(sample_rate: u32, channels: u16, anchor_pts: Pts90) -> Self {
        MasterClock::Audio(AudioClockHandle::new(sample_rate, channels, anchor_pts))
    }

    /// Cria um `MasterClock::Wall` com âncora inicial em `anchor_pts`.
    ///
    /// SPEC-AV-CLOCK-001
    pub fn wall(anchor_pts: Pts90) -> Self {
        MasterClock::Wall(WallClockHandle::new(anchor_pts))
    }

    /// Retorna referência ao `AudioClockHandle` se este for um relógio de áudio.
    ///
    /// SPEC-AV-CLOCK-001
    pub fn audio_handle(&self) -> Option<&AudioClockHandle> {
        match self {
            MasterClock::Audio(h) => Some(h),
            MasterClock::Wall(_) => None,
        }
    }
}

impl Clock for MasterClock {
    fn now_pts90(&self) -> Pts90 {
        match self {
            MasterClock::Audio(h) => h.now_pts90(),
            MasterClock::Wall(h) => h.now_pts90(),
        }
    }

    fn reset(&self, anchor_pts: Pts90) {
        match self {
            MasterClock::Audio(h) => h.reset(anchor_pts),
            MasterClock::Wall(h) => h.reset(anchor_pts),
        }
    }
}

// ─── Testes ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    // ── AudioClockHandle ──────────────────────────────────────────────────────

    /// Relógio de áudio retorna a âncora inicial quando nenhuma amostra foi
    /// consumida.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_zero_samples_returns_anchor() {
        let clock = AudioClockHandle::new(48_000, 2, 900_000);
        assert_eq!(clock.now_pts90(), 900_000);
    }

    /// Incremento correto: 48 000 samples estéreo (= 48 000 frames) em 48 kHz
    /// equivale a 1 segundo → offset de 90 000 unidades 90 kHz.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_one_second_offset() {
        let clock = AudioClockHandle::new(48_000, 2, 0);
        // 1 segundo de áudio estéreo = 48 000 frames × 2 canais = 96 000 samples
        clock.samples_played.store(48_000 * 2, Ordering::Relaxed);
        assert_eq!(clock.now_pts90(), 90_000);
    }

    /// Âncora não-zero é somada corretamente ao offset calculado.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_nonzero_anchor() {
        let anchor: Pts90 = 270_000; // 3 segundos na âncora
        let clock = AudioClockHandle::new(48_000, 2, anchor);
        // Mais 1 segundo de áudio
        clock.samples_played.store(48_000 * 2, Ordering::Relaxed);
        assert_eq!(clock.now_pts90(), anchor + 90_000);
    }

    /// `reset()` zera `samples_played` e reposiciona a âncora.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_reset_zeros_counter_and_sets_anchor() {
        let clock = AudioClockHandle::new(48_000, 2, 0);
        // Simula 5 segundos de áudio
        clock
            .samples_played
            .store(48_000 * 2 * 5, Ordering::Relaxed);
        clock.reset(180_000); // nova âncora = 2 s
        assert_eq!(clock.samples_played.load(Ordering::Relaxed), 0);
        assert_eq!(clock.now_pts90(), 180_000);
    }

    /// `samples_counter()` retorna um `Arc` compartilhado com o contador
    /// interno; incrementar via o Arc reflete em `now_pts90()`.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_samples_counter_arc_shared() {
        let clock = AudioClockHandle::new(48_000, 1, 0);
        let counter = clock.samples_counter();
        // Simula callback cpal: 48 000 samples mono = 1 s
        counter.fetch_add(48_000, Ordering::Relaxed);
        assert_eq!(clock.now_pts90(), 90_000);
    }

    /// Thread-safety: incremento concorrente de `samples_played` deve
    /// resultar em valor coerente (não precisa ser exato por `Relaxed`).
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_concurrent_increment() {
        let clock = Arc::new(AudioClockHandle::new(48_000, 1, 0));
        let counter = clock.samples_counter();

        let threads: Vec<_> = (0..4)
            .map(|_| {
                let c = Arc::clone(&counter);
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }

        // 4 threads × 1000 = 4000 samples mono em 48 kHz → ~7.5 ms
        let played = counter.load(Ordering::Relaxed);
        assert_eq!(played, 4_000);
        // `now_pts90` deve ser coerente (não negativo, não enorme)
        let pts = clock.now_pts90();
        assert!(pts >= 0, "pts deve ser não-negativo, got {pts}");
    }

    /// Canais = 0 não causa pânico; retorna a âncora.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_zero_channels_safe() {
        let clock = AudioClockHandle::new(48_000, 0, 12345);
        clock.samples_played.store(99_999, Ordering::Relaxed);
        assert_eq!(clock.now_pts90(), 12345);
    }

    /// Sample rate = 0 não causa pânico; retorna a âncora.
    ///
    /// SPEC-AV-CLOCK-002
    #[test]
    fn spec_av_clock_002_audio_clock_zero_sample_rate_safe() {
        let clock = AudioClockHandle::new(0, 2, 12345);
        clock.samples_played.store(99_999, Ordering::Relaxed);
        assert_eq!(clock.now_pts90(), 12345);
    }

    // ── WallClockHandle ───────────────────────────────────────────────────────

    /// `WallClockHandle` começa na âncora (tempo decorrido ≈ 0).
    ///
    /// SPEC-AV-CLOCK-003
    #[test]
    fn spec_av_clock_003_wall_clock_starts_near_anchor() {
        let anchor: Pts90 = 900_000;
        let clock = WallClockHandle::new(anchor);
        // Tempo decorrido desde criação é ínfimo; pts deve estar muito perto da âncora
        let pts = clock.now_pts90();
        // Tolerância de 1 segundo = 90 000 unidades
        assert!(
            pts >= anchor && pts < anchor + 90_000,
            "pts={pts} âncora={anchor}"
        );
    }

    /// `reset()` reposiciona o relógio; após reset, `now_pts90` deve estar
    /// perto da nova âncora.
    ///
    /// SPEC-AV-CLOCK-003
    #[test]
    fn spec_av_clock_003_wall_clock_reset_repositions() {
        let clock = WallClockHandle::new(0);
        thread::sleep(Duration::from_millis(5));
        clock.reset(500_000);
        let pts = clock.now_pts90();
        // Após reset, tempo decorrido é ínfimo
        assert!(
            pts >= 500_000 && pts < 500_000 + 90_000,
            "pts={pts} após reset"
        );
    }

    /// `WallClockHandle` avança com o tempo real.
    ///
    /// SPEC-AV-CLOCK-003
    #[test]
    fn spec_av_clock_003_wall_clock_advances_over_time() {
        let clock = WallClockHandle::new(0);
        let before = clock.now_pts90();
        thread::sleep(Duration::from_millis(20));
        let after = clock.now_pts90();
        // 20 ms = 1 800 unidades 90 kHz; tolerância 100 ms = 9 000
        assert!(
            after > before,
            "pts deve avançar com o tempo: before={before} after={after}"
        );
        assert!(
            after - before < 9_000,
            "avanço excessivo: Δpts={} (máx 9000)",
            after - before
        );
    }

    // ── MasterClock ───────────────────────────────────────────────────────────

    /// `MasterClock::Audio` delega corretamente para `AudioClockHandle`.
    ///
    /// SPEC-AV-CLOCK-001
    #[test]
    fn spec_av_clock_001_master_clock_audio_delegates() {
        let mc = MasterClock::audio(48_000, 2, 0);
        if let Some(h) = mc.audio_handle() {
            h.samples_played.store(48_000 * 2, Ordering::Relaxed);
        }
        assert_eq!(mc.now_pts90(), 90_000);
    }

    /// `MasterClock::Wall` delega corretamente para `WallClockHandle`.
    ///
    /// SPEC-AV-CLOCK-001
    #[test]
    fn spec_av_clock_001_master_clock_wall_delegates() {
        let mc = MasterClock::wall(270_000);
        let pts = mc.now_pts90();
        assert!(pts >= 270_000 && pts < 270_000 + 90_000);
    }

    /// `MasterClock::audio_handle()` retorna `Some` para Audio e `None` para Wall.
    ///
    /// SPEC-AV-CLOCK-001
    #[test]
    fn spec_av_clock_001_audio_handle_some_and_none() {
        let audio = MasterClock::audio(48_000, 2, 0);
        assert!(audio.audio_handle().is_some());
        let wall = MasterClock::wall(0);
        assert!(wall.audio_handle().is_none());
    }

    /// `MasterClock::reset()` é delegado corretamente.
    ///
    /// SPEC-AV-CLOCK-001
    #[test]
    fn spec_av_clock_001_master_clock_reset_delegates() {
        let mc = MasterClock::audio(48_000, 2, 0);
        if let Some(h) = mc.audio_handle() {
            h.samples_played.store(48_000 * 2 * 10, Ordering::Relaxed);
        }
        mc.reset(999_000);
        assert_eq!(mc.now_pts90(), 999_000);
    }
}
