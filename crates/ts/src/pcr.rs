/// Rastreador de jitter e descontinuidade PCR por PID.
///
/// SPEC-TS-004b
use std::collections::HashMap;
use std::time::Instant;

use crossbeam_channel::Sender;

use crate::error::{DiscontinuityReason, PcrEvent};
use crate::Pid;

/// Máscara de 42 bits para wrap-around do PCR.
const PCR_MASK: u64 = (1u64 << 42) - 1;

/// Threshold de jitter: 500 µs.
const JITTER_THRESHOLD_US: i64 = 500;

/// Threshold de salto grande de PCR: 100 ms em µs.
const LARGE_JUMP_THRESHOLD_US: i64 = 100_000;

/// Estado interno por PID.
struct PcrState {
    last_pcr: u64,
    last_time: Instant,
}

/// Rastreador de jitter e descontinuidade PCR por PID.
///
/// Para cada PID com PCR, mantém o último valor de PCR e o instante
/// em que foi recebido. A cada novo PCR, calcula o jitter e emite
/// `PcrEvent` quando os thresholds são ultrapassados.
///
/// SPEC-TS-004b
pub struct PcrTracker {
    state: HashMap<Pid, PcrState>,
    event_tx: Sender<PcrEvent>,
}

impl PcrTracker {
    /// Cria um novo `PcrTracker` que envia eventos para `event_tx`.
    ///
    /// SPEC-TS-004b
    pub fn new(event_tx: Sender<PcrEvent>) -> Self {
        Self {
            state: HashMap::new(),
            event_tx,
        }
    }

    /// Processa um novo valor PCR para o PID especificado.
    ///
    /// - Se `discontinuity_indicator` estiver setado, emite
    ///   `PcrEvent::Discontinuity { reason: Flag }` e reseta o estado.
    /// - Se o delta PCR exceder 100 ms, emite
    ///   `PcrEvent::Discontinuity { reason: LargeJump }`.
    /// - Se o jitter (|delta_real - delta_pcr|) exceder 500 µs, emite
    ///   `PcrEvent::Jitter`.
    ///
    /// SPEC-TS-004b
    pub fn update(&mut self, pid: Pid, pcr: u64, discontinuity_indicator: bool) {
        self.update_with_time(pid, pcr, discontinuity_indicator, Instant::now());
    }

    /// Versão com injeção de tempo para testes determinísticos.
    ///
    /// SPEC-TS-004b
    pub(crate) fn update_with_time(
        &mut self,
        pid: Pid,
        pcr: u64,
        discontinuity_indicator: bool,
        now: Instant,
    ) {
        // Extraímos os valores antes de qualquer mutação para satisfazer o borrow checker.
        let prev = self.state.get(&pid).map(|s| (s.last_pcr, s.last_time));

        if let Some((last_pcr, last_time)) = prev {
            if discontinuity_indicator {
                let _ = self.event_tx.try_send(PcrEvent::Discontinuity {
                    pid,
                    reason: DiscontinuityReason::Flag,
                });
            } else {
                // Delta com wrap-around de 42 bits (PCR: base*300 + ext, max ≈ 4,4 × 10¹²).
                let delta_pcr_ticks = pcr.wrapping_sub(last_pcr) & PCR_MASK;
                // 27 MHz → µs: dividir por 27.
                let delta_pcr_us = (delta_pcr_ticks as f64 / 27.0) as i64;

                if delta_pcr_us > LARGE_JUMP_THRESHOLD_US {
                    let delta_ms = (delta_pcr_us / 1_000) as u64;
                    let _ = self.event_tx.try_send(PcrEvent::Discontinuity {
                        pid,
                        reason: DiscontinuityReason::LargeJump { delta_ms },
                    });
                } else {
                    let delta_real_us = now.duration_since(last_time).as_micros() as i64;
                    let jitter_us = (delta_real_us - delta_pcr_us).abs();

                    if jitter_us > JITTER_THRESHOLD_US {
                        let _ = self.event_tx.try_send(PcrEvent::Jitter {
                            pid,
                            expected_us: delta_pcr_us,
                            measured_us: delta_real_us,
                        });
                    }
                }
            }
        }

        // Sempre atualiza o estado com o novo PCR (inclusive após descontinuidade).
        self.state.insert(
            pid,
            PcrState {
                last_pcr: pcr,
                last_time: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossbeam_channel::bounded;

    use super::*;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_tracker() -> (PcrTracker, crossbeam_channel::Receiver<PcrEvent>) {
        let (tx, rx) = bounded(32);
        (PcrTracker::new(tx), rx)
    }

    /// Converte µs em ticks PCR (27 MHz).
    fn us_to_ticks(us: u64) -> u64 {
        us * 27
    }

    // ── testes ───────────────────────────────────────────────────────────────

    /// Primeiro PCR de um PID não emite nenhum evento.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_first_pcr_no_event() {
        let (mut tracker, rx) = make_tracker();
        let t0 = Instant::now();

        tracker.update_with_time(100, 0, false, t0);

        assert!(
            rx.try_recv().is_err(),
            "nenhum evento esperado no primeiro PCR"
        );
    }

    /// Jitter acima de 500 µs deve emitir `PcrEvent::Jitter`.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_pcr_jitter_threshold() {
        let (mut tracker, rx) = make_tracker();
        let pid = 200u16;

        let t0 = Instant::now();
        // PCR avança 1 000 µs (27 000 ticks), mas tempo real avança 2 000 µs.
        // Jitter = |2000 - 1000| = 1000 µs > 500 µs → deve emitir Jitter.
        let pcr0 = 0u64;
        let pcr1 = us_to_ticks(1_000);
        let t1 = t0 + Duration::from_micros(2_000);

        tracker.update_with_time(pid, pcr0, false, t0);
        tracker.update_with_time(pid, pcr1, false, t1);

        let ev = rx.try_recv().expect("esperava PcrEvent::Jitter");
        match ev {
            PcrEvent::Jitter {
                pid: p,
                expected_us,
                measured_us,
            } => {
                assert_eq!(p, pid);
                assert_eq!(expected_us, 1_000);
                assert_eq!(measured_us, 2_000);
            }
            other => panic!("evento inesperado: {other:?}"),
        }
    }

    /// Jitter abaixo de 500 µs não emite evento.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_pcr_jitter_below_threshold_no_event() {
        let (mut tracker, rx) = make_tracker();
        let pid = 201u16;

        let t0 = Instant::now();
        // PCR: 1 000 µs; real: 1 200 µs → jitter = 200 µs < 500 µs.
        let pcr0 = 0u64;
        let pcr1 = us_to_ticks(1_000);
        let t1 = t0 + Duration::from_micros(1_200);

        tracker.update_with_time(pid, pcr0, false, t0);
        tracker.update_with_time(pid, pcr1, false, t1);

        assert!(
            rx.try_recv().is_err(),
            "nenhum evento esperado com jitter baixo"
        );
    }

    /// `discontinuity_indicator = true` emite `PcrEvent::Discontinuity { reason: Flag }`.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_pcr_discontinuity_flag() {
        let (mut tracker, rx) = make_tracker();
        let pid = 300u16;

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_micros(1_000);

        tracker.update_with_time(pid, 0, false, t0);
        tracker.update_with_time(pid, us_to_ticks(1_000), true, t1);

        let ev = rx.try_recv().expect("esperava PcrEvent::Discontinuity");
        match ev {
            PcrEvent::Discontinuity {
                pid: p,
                reason: DiscontinuityReason::Flag,
            } => {
                assert_eq!(p, pid);
            }
            other => panic!("evento inesperado: {other:?}"),
        }
    }

    /// Após uma descontinuidade sinalizada, o próximo PCR deve usar a nova
    /// baseline e não gerar falso positivo de jitter.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_discontinuity_resets_baseline_for_following_pcr() {
        let (mut tracker, rx) = make_tracker();
        let pid = 301u16;

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_micros(50_000);
        let t2 = t1 + Duration::from_micros(1_000);

        tracker.update_with_time(pid, 0, false, t0);
        tracker.update_with_time(pid, us_to_ticks(50_000), true, t1);

        let ev = rx.try_recv().expect("esperava PcrEvent::Discontinuity");
        match ev {
            PcrEvent::Discontinuity {
                pid: p,
                reason: DiscontinuityReason::Flag,
            } => assert_eq!(p, pid),
            other => panic!("evento inesperado: {other:?}"),
        }

        tracker.update_with_time(pid, us_to_ticks(51_000), false, t2);

        assert!(
            rx.try_recv().is_err(),
            "após reset de baseline, o PCR seguinte não deve gerar jitter espúrio"
        );
    }

    /// Salto de PCR acima de 100 ms emite `PcrEvent::Discontinuity { reason: LargeJump }`.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_pcr_large_jump() {
        let (mut tracker, rx) = make_tracker();
        let pid = 400u16;

        let t0 = Instant::now();
        // Delta de 111 111 µs ≈ 111 ms > 100 ms → LargeJump.
        let pcr0 = 0u64;
        let pcr1 = us_to_ticks(111_111);
        let t1 = t0 + Duration::from_micros(111_111);

        tracker.update_with_time(pid, pcr0, false, t0);
        tracker.update_with_time(pid, pcr1, false, t1);

        let ev = rx.try_recv().expect("esperava PcrEvent::Discontinuity");
        match ev {
            PcrEvent::Discontinuity {
                pid: p,
                reason: DiscontinuityReason::LargeJump { delta_ms },
            } => {
                assert_eq!(p, pid);
                assert_eq!(delta_ms, 111);
            }
            other => panic!("evento inesperado: {other:?}"),
        }
    }

    /// PIDs independentes são rastreados separadamente.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_independent_pids() {
        let (mut tracker, rx) = make_tracker();

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_micros(2_000);

        // PID 500: jitter alto → evento
        tracker.update_with_time(500, 0, false, t0);
        tracker.update_with_time(500, us_to_ticks(1_000), false, t1);

        // PID 501: jitter baixo → sem evento
        tracker.update_with_time(501, 0, false, t0);
        tracker.update_with_time(501, us_to_ticks(2_000), false, t1);

        // Deve haver exatamente 1 evento, referente ao PID 500.
        let ev = rx.try_recv().expect("esperava 1 evento");
        match ev {
            PcrEvent::Jitter { pid: 500, .. } => {}
            other => panic!("evento inesperado: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "não deve haver segundo evento");
    }

    /// Após descontinuidade, o estado é resetado: próximo PCR não emite evento
    /// baseado no valor anterior à descontinuidade.
    ///
    /// SPEC-TS-004b
    #[test]
    fn spec_ts_004b_state_reset_after_discontinuity() {
        let (mut tracker, rx) = make_tracker();
        let pid = 600u16;

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_micros(1_000);
        let t2 = t1 + Duration::from_micros(1_000);

        // Primeiro PCR.
        tracker.update_with_time(pid, 0, false, t0);
        // Descontinuidade: reseta estado.
        tracker.update_with_time(pid, us_to_ticks(1_000), true, t1);
        // PCR pós-reset com jitter normal (não baseado no PCR anterior à descontinuidade).
        tracker.update_with_time(pid, us_to_ticks(2_000), false, t2);

        // Apenas 1 evento: a descontinuidade.
        let ev = rx.try_recv().expect("esperava 1 evento");
        assert!(
            matches!(
                ev,
                PcrEvent::Discontinuity {
                    reason: DiscontinuityReason::Flag,
                    ..
                }
            ),
            "esperava Discontinuity::Flag, recebeu: {ev:?}"
        );
        assert!(rx.try_recv().is_err(), "não deve haver segundo evento");
    }
}
