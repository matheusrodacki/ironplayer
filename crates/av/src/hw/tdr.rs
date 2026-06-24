//! TDR (Timeout Detection and Recovery) — máquina de estado de recuperação
//! de eventos `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_RESET`.
//!
//! Esta camada é **platform-agnostic** e contém apenas a lógica de decisão:
//! quando tentar recuperar, quando desistir, quanto tempo esperar entre
//! tentativas.  A invocação real (`D3D11CreateDevice`, drain do `VideoQueue`,
//! reinicialização do `FfmpegDecoder`) é feita pelo caller na thread de
//! render quando este componente sinaliza `should_attempt`.
//!
//! Referências:
//! - TDD Sprint 2 §5 R8 (Reset de device invalida texturas vivas).
//! - TDD Sprint 2 §6 (E2E-6 — TDR recovery em < 2 s).
//! - TDD Sprint 2 §8 (métrica `tdr_recoveries`).
//!
//! SPEC-AV-HW-TDR-001

use std::time::{Duration, Instant};

/// Cooldown mínimo entre tentativas de recuperação de TDR.
///
/// Evita laço apertado quando o driver volta `DEVICE_REMOVED` imediatamente
/// na primeira chamada após o reset.  Valor escolhido para ficar bem abaixo
/// do orçamento de 2 s exigido pelo critério E2E-6.
///
/// SPEC-AV-HW-TDR-001
pub const TDR_RETRY_COOLDOWN: Duration = Duration::from_millis(250);

/// Número máximo de tentativas consecutivas antes de cair em fallback CPU
/// permanente para a sessão.
///
/// SPEC-AV-HW-TDR-001
pub const TDR_MAX_ATTEMPTS: u32 = 4;

/// Estado de uma sessão de recuperação de TDR.
///
/// O caller (camada `av`/`ui` na thread de render) consulta `should_attempt`
/// para saber se deve tentar `D3D11CreateDevice` novamente; a cada tentativa
/// registra resultado via `record_attempt`.
///
/// Variantes:
/// - `Healthy`: nenhum TDR observado.
/// - `Recovering`: TDR sinalizado; aguarda cooldown e tenta recriar device.
/// - `Failed`: máximo de tentativas atingido; rebaixamento permanente
///   para CPU; o caller deve chamar `FfmpegDecoder::fallback_to_sw`.
///
/// SPEC-AV-HW-TDR-001
#[derive(Debug, Clone)]
pub struct TdrState {
    phase: TdrPhase,
    /// Número total de recoveries bem-sucedidos na sessão.
    successful_recoveries: u64,
    /// Tentativas consecutivas em curso (zera após sucesso).
    consecutive_attempts: u32,
    /// Último carimbo de tempo em que tentamos recuperar (para cooldown).
    last_attempt: Option<Instant>,
    /// Razão da última falha permanente (preservada para telemetria).
    last_failure_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TdrPhase {
    Healthy,
    Recovering,
    Failed,
}

impl Default for TdrState {
    fn default() -> Self {
        Self {
            phase: TdrPhase::Healthy,
            successful_recoveries: 0,
            consecutive_attempts: 0,
            last_attempt: None,
            last_failure_reason: None,
        }
    }
}

impl TdrState {
    /// Constrói um estado inicial saudável.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sinaliza que o caller observou um evento TDR (`DEVICE_REMOVED`).
    ///
    /// Idempotente — chamadas adicionais em estado `Recovering` apenas
    /// preservam o contador.
    ///
    /// SPEC-AV-HW-TDR-001
    pub fn signal_device_removed(&mut self) {
        if self.phase == TdrPhase::Failed {
            return;
        }
        self.phase = TdrPhase::Recovering;
    }

    /// Retorna `true` se o caller deve tentar recriar o device agora.
    ///
    /// Considera cooldown desde a última tentativa e o limite máximo.
    ///
    /// SPEC-AV-HW-TDR-001
    pub fn should_attempt(&self, now: Instant) -> bool {
        if self.phase != TdrPhase::Recovering {
            return false;
        }
        if self.consecutive_attempts >= TDR_MAX_ATTEMPTS {
            return false;
        }
        match self.last_attempt {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= TDR_RETRY_COOLDOWN,
        }
    }

    /// Registra uma tentativa bem-sucedida de recuperação.
    ///
    /// Reseta contadores e volta ao estado `Healthy`; incrementa
    /// `successful_recoveries` para a métrica `tdr_recoveries`.
    ///
    /// SPEC-AV-HW-TDR-001
    pub fn record_success(&mut self, now: Instant) {
        self.successful_recoveries = self.successful_recoveries.saturating_add(1);
        self.consecutive_attempts = 0;
        self.last_attempt = Some(now);
        self.last_failure_reason = None;
        self.phase = TdrPhase::Healthy;
    }

    /// Registra uma tentativa falha (driver continua retornando erro).
    ///
    /// Retorna `true` quando o limite máximo foi atingido e o caller deve
    /// transitar para fallback CPU permanente.
    ///
    /// SPEC-AV-HW-TDR-001
    #[must_use = "verifique o retorno para acionar fallback definitivo"]
    pub fn record_failure(&mut self, now: Instant, reason: impl Into<String>) -> bool {
        if self.phase == TdrPhase::Failed {
            return true;
        }
        self.phase = TdrPhase::Recovering;
        self.consecutive_attempts = self.consecutive_attempts.saturating_add(1);
        self.last_attempt = Some(now);
        self.last_failure_reason = Some(reason.into());
        if self.consecutive_attempts >= TDR_MAX_ATTEMPTS {
            self.phase = TdrPhase::Failed;
            true
        } else {
            false
        }
    }

    /// Total de recoveries bem-sucedidos na sessão (alimenta `tdr_recoveries`).
    pub fn successful_recoveries(&self) -> u64 {
        self.successful_recoveries
    }

    /// `true` quando o caller já entrou em fallback permanente nesta sessão.
    pub fn is_failed(&self) -> bool {
        self.phase == TdrPhase::Failed
    }

    /// `true` quando o caller está no meio de uma sessão de recuperação.
    pub fn is_recovering(&self) -> bool {
        self.phase == TdrPhase::Recovering
    }

    /// Razão da última falha (preservada para log estruturado).
    pub fn last_failure_reason(&self) -> Option<&str> {
        self.last_failure_reason.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Estado inicial é healthy e não tem recoveries.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_initial_state_is_healthy() {
        let s = TdrState::new();
        assert!(!s.is_recovering());
        assert!(!s.is_failed());
        assert_eq!(s.successful_recoveries(), 0);
        assert!(s.last_failure_reason().is_none());
        // Sem TDR sinalizado: nenhuma tentativa deve ser feita.
        assert!(!s.should_attempt(Instant::now()));
    }

    /// signal_device_removed transita Healthy → Recovering e libera a
    /// primeira tentativa imediatamente (sem cooldown).
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_signal_allows_immediate_attempt() {
        let mut s = TdrState::new();
        s.signal_device_removed();
        assert!(s.is_recovering());
        assert!(s.should_attempt(Instant::now()));
    }

    /// Cooldown bloqueia retries imediatos.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_cooldown_blocks_retry() {
        let mut s = TdrState::new();
        s.signal_device_removed();
        let t0 = Instant::now();
        let bumped = s.record_failure(t0, "DXGI_ERROR_DEVICE_REMOVED");
        assert!(!bumped, "1ª falha não deve atingir TDR_MAX_ATTEMPTS");
        // Imediatamente após: cooldown ainda ativo.
        assert!(!s.should_attempt(t0));
        // Depois do cooldown: liberado.
        assert!(s.should_attempt(t0 + TDR_RETRY_COOLDOWN));
    }

    /// record_success zera contadores, volta a Healthy e incrementa o
    /// contador de recoveries cumulativos.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_success_resets_and_increments() {
        let mut s = TdrState::new();
        s.signal_device_removed();
        let t0 = Instant::now();
        let _ = s.record_failure(t0, "transitório");
        s.record_success(t0 + TDR_RETRY_COOLDOWN);
        assert!(!s.is_recovering());
        assert!(!s.is_failed());
        assert_eq!(s.successful_recoveries(), 1);
        assert!(s.last_failure_reason().is_none());
    }

    /// Atingir TDR_MAX_ATTEMPTS transita para Failed e bloqueia novas
    /// tentativas em qualquer instante futuro.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_max_attempts_transitions_to_failed() {
        let mut s = TdrState::new();
        s.signal_device_removed();
        let mut t = Instant::now();
        let mut bumped = false;
        for _ in 0..TDR_MAX_ATTEMPTS {
            bumped = s.record_failure(t, "persistente");
            t += TDR_RETRY_COOLDOWN;
        }
        assert!(bumped, "a última tentativa deve sinalizar fallback");
        assert!(s.is_failed());
        // Mesmo após cooldown, não tentamos mais.
        assert!(!s.should_attempt(t + TDR_RETRY_COOLDOWN * 10));
        assert_eq!(s.last_failure_reason(), Some("persistente"));
    }

    /// Uma vez em Failed, signal_device_removed adicional é no-op.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_failed_is_terminal() {
        let mut s = TdrState::new();
        s.signal_device_removed();
        let mut t = Instant::now();
        for _ in 0..TDR_MAX_ATTEMPTS {
            let _ = s.record_failure(t, "x");
            t += TDR_RETRY_COOLDOWN;
        }
        assert!(s.is_failed());
        s.signal_device_removed();
        assert!(s.is_failed(), "signal não deve sair de Failed");
        // record_failure subsequente retorna true mas não muda estado.
        assert!(s.record_failure(t, "y"));
        assert!(s.is_failed());
    }

    /// successful_recoveries acumula através de múltiplos ciclos.
    ///
    /// SPEC-AV-HW-TDR-001
    #[test]
    fn spec_av_hw_tdr_001_successful_recoveries_accumulate() {
        let mut s = TdrState::new();
        let t0 = Instant::now();
        for i in 1..=3 {
            s.signal_device_removed();
            s.record_success(t0 + Duration::from_millis(i * 100));
        }
        assert_eq!(s.successful_recoveries(), 3);
        assert!(!s.is_recovering());
    }
}
