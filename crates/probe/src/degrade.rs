//! Controlador de degradação sob sobrecarga.
//!
//! SPEC-PROBE-013a — a probe degrada nesta ordem: 1) thumbnail, 2) rollups de
//! UI, 3) séries secundárias.  A recepção UDP e a contagem de perda/CC **nunca**
//! são as primeiras a degradar — nada aqui as toca.
//!
//! SPEC-PROBE-013b — a degradação é registrada como evento informativo
//! (`probe_degraded`), não silenciosa; quem emite é o [`crate::ProbeEngine`], a
//! partir do estágio devolvido aqui.

use std::time::Duration;

use crate::snapshot::DegradationStage;

/// Sinais de sobrecarga observados num tick.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OverloadSignals {
    /// Atraso do tick em relação ao agendado, em ms.
    pub sched_jitter_ms: f64,
    /// Descartes locais novos neste tick (canal cheio, socket).
    pub local_drops_delta: u64,
    /// Linhas de disco descartadas por fila cheia neste tick.
    pub writer_drops_delta: u64,
}

impl OverloadSignals {
    /// `true` quando algum sinal indica que a probe não está acompanhando.
    fn overloaded(&self, jitter_limit_ms: f64) -> bool {
        self.sched_jitter_ms > jitter_limit_ms
            || self.local_drops_delta > 0
            || self.writer_drops_delta > 0
    }
}

/// Histerese da degradação.
///
/// Subir é rápido (a sobrecarga já está acontecendo); descer é lento, para não
/// ficar oscilando entre estágios num notebook com carga irregular.
#[derive(Debug, Clone, Copy)]
pub struct DegradePolicy {
    /// Acima deste atraso de tick, considera-se sobrecarga.
    pub jitter_limit_ms: f64,
    /// Ticks consecutivos sobrecarregados antes de subir um estágio.
    pub escalate_after: u32,
    /// Ticks consecutivos saudáveis antes de descer um estágio.
    pub recover_after: u32,
}

impl Default for DegradePolicy {
    fn default() -> Self {
        Self {
            // Metade do período de amostragem: a partir daí o tick de 1 Hz já
            // não é mais 1 Hz de verdade.
            jitter_limit_ms: 500.0,
            escalate_after: 3,
            recover_after: 30,
        }
    }
}

impl DegradePolicy {
    /// Deriva a política a partir do período de amostragem configurado.
    ///
    /// SPEC-PROBE-013a
    pub fn for_interval(interval: Duration) -> Self {
        Self {
            jitter_limit_ms: (interval.as_secs_f64() * 1000.0 * 0.5).max(100.0),
            ..Self::default()
        }
    }
}

/// Máquina de estados da degradação.
///
/// SPEC-PROBE-013a
#[derive(Debug, Clone)]
pub struct DegradeController {
    policy: DegradePolicy,
    stage: DegradationStage,
    overloaded_ticks: u32,
    healthy_ticks: u32,
}

impl DegradeController {
    /// Cria o controlador no estágio normal.
    pub fn new(policy: DegradePolicy) -> Self {
        Self {
            policy,
            stage: DegradationStage::None,
            overloaded_ticks: 0,
            healthy_ticks: 0,
        }
    }

    /// Estágio corrente.
    pub fn stage(&self) -> DegradationStage {
        self.stage
    }

    /// Processa um tick e devolve o estágio resultante.
    ///
    /// SPEC-PROBE-013a
    pub fn observe(&mut self, signals: OverloadSignals) -> DegradationStage {
        if signals.overloaded(self.policy.jitter_limit_ms) {
            self.healthy_ticks = 0;
            self.overloaded_ticks = self.overloaded_ticks.saturating_add(1);
            if self.overloaded_ticks >= self.policy.escalate_after {
                self.overloaded_ticks = 0;
                self.stage = escalate(self.stage);
            }
        } else {
            self.overloaded_ticks = 0;
            self.healthy_ticks = self.healthy_ticks.saturating_add(1);
            if self.healthy_ticks >= self.policy.recover_after {
                self.healthy_ticks = 0;
                self.stage = relax(self.stage);
            }
        }
        self.stage
    }
}

fn escalate(stage: DegradationStage) -> DegradationStage {
    match stage {
        DegradationStage::None => DegradationStage::Thumbnail,
        DegradationStage::Thumbnail => DegradationStage::Rollups,
        // Não há estágio além do 3º: degradar mais atingiria a recepção UDP e
        // a contagem de perda/CC, que SPEC-PROBE-013a protege explicitamente.
        _ => DegradationStage::SecondarySeries,
    }
}

fn relax(stage: DegradationStage) -> DegradationStage {
    match stage {
        DegradationStage::SecondarySeries => DegradationStage::Rollups,
        DegradationStage::Rollups => DegradationStage::Thumbnail,
        _ => DegradationStage::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jitter(ms: f64) -> OverloadSignals {
        OverloadSignals {
            sched_jitter_ms: ms,
            ..Default::default()
        }
    }

    fn healthy() -> OverloadSignals {
        OverloadSignals::default()
    }

    /// SPEC-PROBE-013a — a degradação sobe na ordem exigida pela spec.
    #[test]
    fn spec_probe_013a_escalates_in_specified_order() {
        let mut c = DegradeController::new(DegradePolicy::default());
        for _ in 0..2 {
            assert_eq!(c.observe(jitter(900.0)), DegradationStage::None);
        }
        assert_eq!(c.observe(jitter(900.0)), DegradationStage::Thumbnail);

        for _ in 0..2 {
            c.observe(jitter(900.0));
        }
        assert_eq!(c.observe(jitter(900.0)), DegradationStage::Rollups);

        for _ in 0..2 {
            c.observe(jitter(900.0));
        }
        assert_eq!(c.observe(jitter(900.0)), DegradationStage::SecondarySeries);
    }

    /// SPEC-PROBE-013a — o 3º estágio é o teto: degradar mais atingiria a
    /// recepção, que a spec protege.
    #[test]
    fn spec_probe_013a_never_degrades_beyond_third_stage() {
        let mut c = DegradeController::new(DegradePolicy::default());
        for _ in 0..200 {
            c.observe(jitter(5_000.0));
        }
        assert_eq!(c.stage(), DegradationStage::SecondarySeries);
    }

    /// SPEC-PROBE-013a — recuperar é lento e volta um estágio de cada vez.
    #[test]
    fn spec_probe_013a_recovery_is_gradual() {
        let mut c = DegradeController::new(DegradePolicy {
            escalate_after: 1,
            recover_after: 3,
            ..Default::default()
        });
        c.observe(jitter(900.0));
        c.observe(jitter(900.0));
        assert_eq!(c.stage(), DegradationStage::Rollups);

        for _ in 0..2 {
            assert_eq!(c.observe(healthy()), DegradationStage::Rollups);
        }
        assert_eq!(c.observe(healthy()), DegradationStage::Thumbnail);
        for _ in 0..2 {
            c.observe(healthy());
        }
        assert_eq!(c.observe(healthy()), DegradationStage::None);
    }

    /// SPEC-PROBE-013a — descarte local sozinho já caracteriza sobrecarga,
    /// mesmo com o tick pontual.
    #[test]
    fn spec_probe_013a_local_drops_alone_trigger_degradation() {
        let mut c = DegradeController::new(DegradePolicy {
            escalate_after: 1,
            ..Default::default()
        });
        let signals = OverloadSignals {
            local_drops_delta: 1,
            ..Default::default()
        };
        assert_eq!(c.observe(signals), DegradationStage::Thumbnail);
    }

    /// SPEC-PROBE-013a — o limiar de jitter acompanha o período configurado.
    #[test]
    fn spec_probe_013a_policy_scales_with_sample_interval() {
        let p = DegradePolicy::for_interval(Duration::from_secs(1));
        assert!((p.jitter_limit_ms - 500.0).abs() < f64::EPSILON);

        // Períodos muito curtos não derrubam o limiar a zero.
        let p = DegradePolicy::for_interval(Duration::from_millis(100));
        assert!(p.jitter_limit_ms >= 100.0);
    }

    /// SPEC-PROBE-013a — sem sobrecarga, nada degrada.
    #[test]
    fn spec_probe_013a_healthy_probe_never_degrades() {
        let mut c = DegradeController::new(DegradePolicy::default());
        for _ in 0..1_000 {
            assert_eq!(c.observe(healthy()), DegradationStage::None);
        }
    }
}
