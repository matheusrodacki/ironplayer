//! `HwAccelMode` — seleção de hardware acceleration e máquina de fallback.
//!
//! Esta camada é **platform-agnostic** (vive em todos os builds) e contém a
//! lógica de decisão entre decode acelerado por GPU e fallback CPU.  O caminho
//! D3D11VA real é injetado via `Arc<D3d11Device>` (Windows-only); em outras
//! plataformas, apenas o variant `Off` é construível na prática.
//!
//! Referências:
//! - TDD Sprint 2 §4.2.3 (FfmpegDecoder + HwAccelMode)
//! - TDD Sprint 2 §4.3 (limiar de 3 falhas seguidas → fallback)
//!
//! SPEC-AV-HW-DEC-001

use std::sync::Arc;

use super::D3d11Device;

/// Modo de aceleração de hardware solicitado para um decoder.
///
/// - `Off`: decode 100 % CPU (sws_scale para RGB24), comportamento pré-Sprint 2.
/// - `D3d11Va`: solicita decode D3D11VA usando o `ID3D11Device` compartilhado.
///   Falhas de inicialização ou frames inconsistentes acionam fallback
///   automático ao caminho `Off` (cf. `HwAccelState`).
///
/// SPEC-AV-HW-DEC-001
#[derive(Clone)]
pub enum HwAccelMode {
    /// Decode software clássico.
    Off,
    /// Decode D3D11VA com o device compartilhado.
    D3d11Va(Arc<D3d11Device>),
}

impl std::fmt::Debug for HwAccelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "HwAccelMode::Off"),
            Self::D3d11Va(_) => write!(f, "HwAccelMode::D3d11Va(<device>)"),
        }
    }
}

impl HwAccelMode {
    /// Retorna `true` quando o modo solicita decode em GPU.
    pub fn is_gpu(&self) -> bool {
        matches!(self, Self::D3d11Va(_))
    }

    /// Identificador estável para logs / métricas.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::D3d11Va(_) => "d3d11va",
        }
    }
}

/// Limite de falhas seguidas de decode hwaccel antes de cair para CPU.
///
/// TDD §4.3 — "Threshold de fallback automático = 3 falhas seguidas".
///
/// SPEC-AV-HW-DEC-001
pub const HW_FALLBACK_THRESHOLD: u32 = 3;

/// Estado da máquina de fallback hwaccel mantida pelo decoder.
///
/// Encapsula a contagem de falhas consecutivas e o motivo persistido após
/// rebaixamento.  Uma vez em modo fallback, **não** se promove automaticamente
/// de volta para GPU — a promoção exige reset/reabertura do stream.
///
/// SPEC-AV-HW-DEC-001
#[derive(Debug, Clone, Default)]
pub struct HwAccelState {
    /// `true` enquanto o decoder está produzindo frames `Hw(...)` válidos.
    active: bool,
    /// Falhas consecutivas no caminho hwaccel desde o último frame OK.
    consecutive_failures: u32,
    /// Última razão de fallback registrada (preservada para telemetria).
    fallback_reason: Option<String>,
}

impl HwAccelState {
    /// Estado inicial (CPU; nenhum fallback ainda).
    pub fn new() -> Self {
        Self::default()
    }

    /// Marca que o decoder foi inicializado com sucesso em GPU.
    ///
    /// Reseta contadores e limpa `fallback_reason`.
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn activate(&mut self) {
        self.active = true;
        self.consecutive_failures = 0;
        self.fallback_reason = None;
    }

    /// Registra uma falha hwaccel transitória.  Retorna `true` se a contagem
    /// atingiu o `HW_FALLBACK_THRESHOLD` e o caller deve acionar `fallback`.
    ///
    /// SPEC-AV-HW-DEC-001
    #[must_use = "verifique o retorno para acionar fallback quando necessário"]
    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures >= HW_FALLBACK_THRESHOLD
    }

    /// Marca um frame hwaccel entregue com sucesso (reseta o contador de falhas).
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Promove o decoder para modo fallback CPU, registrando o motivo.
    ///
    /// Idempotente — chamadas subsequentes mantêm o primeiro motivo registrado
    /// para preservar a causa raiz no log estruturado.
    ///
    /// SPEC-AV-HW-DEC-001
    pub fn fallback(&mut self, reason: impl Into<String>) {
        if self.fallback_reason.is_none() {
            self.fallback_reason = Some(reason.into());
        }
        self.active = false;
        self.consecutive_failures = 0;
    }

    /// `true` enquanto o caminho hwaccel está produzindo frames.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Motivo persistido do fallback (se houver).
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// Falhas consecutivas ainda não resolvidas.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Estado inicial: CPU, sem falhas, sem motivo.
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_initial_state_is_cpu() {
        let s = HwAccelState::new();
        assert!(!s.is_active());
        assert_eq!(s.consecutive_failures(), 0);
        assert!(s.fallback_reason().is_none());
    }

    /// Activate promove para GPU e reseta contadores.
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_activate_resets() {
        let mut s = HwAccelState::new();
        let _ = s.record_failure();
        s.fallback("teste");
        s.activate();
        assert!(s.is_active());
        assert_eq!(s.consecutive_failures(), 0);
        assert!(s.fallback_reason().is_none());
    }

    /// 3 falhas seguidas → record_failure retorna true na 3ª.
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_three_failures_trigger_fallback() {
        let mut s = HwAccelState::new();
        s.activate();
        assert!(!s.record_failure(), "1ª falha não deve disparar fallback");
        assert!(!s.record_failure(), "2ª falha não deve disparar fallback");
        assert!(s.record_failure(), "3ª falha deve disparar fallback");
    }

    /// Sucesso entre falhas zera o contador (sem fallback).
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_success_resets_failure_streak() {
        let mut s = HwAccelState::new();
        s.activate();
        let _ = s.record_failure();
        let _ = s.record_failure();
        s.record_success();
        assert_eq!(s.consecutive_failures(), 0);
        assert!(!s.record_failure(), "contador deve ter sido resetado");
    }

    /// fallback persiste a primeira razão e desativa o caminho hwaccel.
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_fallback_persists_first_reason() {
        let mut s = HwAccelState::new();
        s.activate();
        s.fallback("driver ausente");
        s.fallback("outra causa"); // idempotente
        assert!(!s.is_active());
        assert_eq!(s.fallback_reason(), Some("driver ausente"));
    }

    /// HwAccelMode::Off é GPU-off e tem label estável.
    ///
    /// SPEC-AV-HW-DEC-001
    #[test]
    fn spec_av_hw_dec_001_hwaccel_mode_off() {
        let m = HwAccelMode::Off;
        assert!(!m.is_gpu());
        assert_eq!(m.label(), "off");
    }
}
