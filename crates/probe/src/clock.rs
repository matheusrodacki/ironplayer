//! Relógio injetável do modo Probe.
//!
//! RNF-PRB-002 separa dois relógios de propósito diferente:
//!
//! - **monotônico** (`Instant`, QPC no Windows) — usado para tudo que é
//!   intervalo: debounce de check, janela de rollup, uptime, jitter de
//!   agendamento.  Imune a ajuste de NTP e a horário de verão.
//! - **de parede** (`DateTime<Utc>`) — usado **somente** nos timestamps
//!   exportados (`metrics.csv`, `events.jsonl`, nome da pasta de sessão).
//!
//! RNF-PRB-007 exige que o motor de checks aceite fixtures determinísticas.
//! Por isso todo o crate consome [`ProbeClock`] em vez de chamar
//! `Instant::now()` diretamente; os testes usam [`TestClock`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};

/// Fonte de tempo do modo Probe.
///
/// RNF-PRB-002 · RNF-PRB-007
pub trait ProbeClock: Send + Sync {
    /// Instante monotônico — use para medir intervalos.
    fn now_mono(&self) -> Instant;
    /// Relógio de parede UTC — use apenas em timestamps exportados.
    fn now_utc(&self) -> DateTime<Utc>;
}

/// Relógio de produção: `Instant::now()` + `Utc::now()`.
///
/// RNF-PRB-002
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl ProbeClock for SystemClock {
    fn now_mono(&self) -> Instant {
        Instant::now()
    }

    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Relógio controlado por teste: avança apenas quando [`TestClock::advance`]
/// é chamado, de modo que uma sessão de 12 h roda em milissegundos.
///
/// RNF-PRB-007
#[derive(Debug)]
pub struct TestClock {
    base_mono: Instant,
    base_utc: DateTime<Utc>,
    /// Deslocamento acumulado em milissegundos desde a base.
    offset_ms: AtomicU64,
}

impl TestClock {
    /// Cria um relógio parado na época Unix (1970-01-01T00:00:00Z).
    ///
    /// RNF-PRB-007
    pub fn new() -> Self {
        Self {
            base_mono: Instant::now(),
            base_utc: Utc
                .timestamp_opt(0, 0)
                .single()
                .expect("época Unix é um timestamp válido"),
            offset_ms: AtomicU64::new(0),
        }
    }

    /// Avança o relógio (monotônico e de parede em conjunto).
    ///
    /// RNF-PRB-007
    pub fn advance(&self, delta: Duration) {
        self.offset_ms
            .fetch_add(delta.as_millis() as u64, Ordering::SeqCst);
    }

    fn offset(&self) -> Duration {
        Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeClock for TestClock {
    fn now_mono(&self) -> Instant {
        self.base_mono + self.offset()
    }

    fn now_utc(&self) -> DateTime<Utc> {
        self.base_utc
            + chrono::Duration::try_milliseconds(self.offset().as_millis() as i64)
                .unwrap_or_else(chrono::Duration::zero)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RNF-PRB-007 — o relógio de teste avança monotônico e parede juntos.
    #[test]
    fn rnf_prb_007_test_clock_advances_both_scales() {
        let clock = TestClock::new();
        let m0 = clock.now_mono();
        let w0 = clock.now_utc();

        clock.advance(Duration::from_secs(3600));

        assert_eq!(
            clock.now_mono().duration_since(m0),
            Duration::from_secs(3600)
        );
        assert_eq!((clock.now_utc() - w0).num_seconds(), 3600);
    }

    /// RNF-PRB-007 — sem `advance`, leituras repetidas são idênticas
    /// (determinismo de fixture).
    #[test]
    fn rnf_prb_007_test_clock_is_frozen_without_advance() {
        let clock = TestClock::new();
        assert_eq!(clock.now_utc(), clock.now_utc());
        assert_eq!(clock.now_mono(), clock.now_mono());
    }
}
