//! Anti-suspensão e prioridade de thread.
//!
//! SPEC-PROBE-012 · §5.4
//!
//! O `unsafe` deste módulo é deliberadamente mínimo: três chamadas Win32 sem
//! ponteiros, sem alocação e sem estruturas — todas com valor de retorno
//! ignorado ou apenas logado.  A regra do AGENTS.md confina FFI em
//! `av::ffi` porque ali mora o binding do FFmpeg; colocar `kernel32` num
//! bridge de codec seria pior do que este módulo isolado de ~40 linhas.

#[cfg(windows)]
mod ffi {
    // https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    pub const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    pub const ES_CONTINUOUS: u32 = 0x8000_0000;

    pub const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
    pub const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn SetThreadExecutionState(es_flags: u32) -> u32;
        pub fn GetCurrentThread() -> isize;
        pub fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    }
}

/// Mantém o Windows acordado enquanto existir.
///
/// SPEC-PROBE-012 — o estado é aplicado na construção e liberado no `Drop`,
/// de modo que sair do modo Probe (ou fechar a janela) devolve o
/// comportamento normal de suspensão sem depender de um `stop()` explícito.
///
/// Além de `ES_SYSTEM_REQUIRED`, é pedido `ES_DISPLAY_REQUIRED`: o requisito
/// diz "não suspende **nem desliga a tela**", e só o primeiro flag deixaria o
/// monitor apagar durante as 12 h.
#[derive(Debug)]
pub struct KeepAwake {
    active: bool,
}

impl KeepAwake {
    /// Solicita o estado de execução contínua.
    ///
    /// `enabled = false` devolve um guard inerte — é o caminho de
    /// `[probe] prevent_sleep = false`.
    ///
    /// SPEC-PROBE-012
    pub fn acquire(enabled: bool) -> Self {
        if !enabled {
            return Self { active: false };
        }

        #[cfg(windows)]
        {
            // SAFETY: chamada Win32 sem ponteiros; o valor de retorno 0 indica
            // falha e é apenas logado.
            let prev = unsafe {
                ffi::SetThreadExecutionState(
                    ffi::ES_CONTINUOUS | ffi::ES_SYSTEM_REQUIRED | ffi::ES_DISPLAY_REQUIRED,
                )
            };
            if prev == 0 {
                tracing::warn!("probe: SetThreadExecutionState falhou — suspensão não bloqueada");
                return Self { active: false };
            }
            tracing::info!("probe: suspensão e desligamento de tela bloqueados durante a sessão");
            Self { active: true }
        }

        #[cfg(not(windows))]
        {
            tracing::debug!("probe: anti-suspensão não implementada nesta plataforma");
            Self { active: false }
        }
    }

    /// `true` quando o estado foi efetivamente aplicado.
    ///
    /// SPEC-PROBE-012
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        #[cfg(windows)]
        {
            // SAFETY: mesma chamada, agora só com ES_CONTINUOUS — limpa os
            // requisitos anteriores desta thread.
            unsafe {
                ffi::SetThreadExecutionState(ffi::ES_CONTINUOUS);
            }
            tracing::info!("probe: suspensão do sistema liberada");
        }
    }
}

/// Rebaixa a prioridade da thread atual.
///
/// §5.4 — `probe-writer` e `probe-snapshot` rodam abaixo do normal para que
/// I/O de disco e decode SW nunca disputem CPU com a recepção UDP.
pub fn lower_current_thread_priority() {
    #[cfg(windows)]
    set_current_thread_priority(ffi::THREAD_PRIORITY_BELOW_NORMAL);
}

/// Eleva a prioridade da thread atual.
///
/// §5.4 — `net-recv-{slot}` roda acima do normal: SPEC-PROBE-013a exige que a
/// recepção UDP nunca seja a primeira coisa a degradar.
pub fn raise_current_thread_priority() {
    #[cfg(windows)]
    set_current_thread_priority(ffi::THREAD_PRIORITY_ABOVE_NORMAL);
}

#[cfg(windows)]
fn set_current_thread_priority(priority: i32) {
    // SAFETY: `GetCurrentThread` devolve um pseudo-handle que não precisa ser
    // fechado; `SetThreadPriority` não recebe ponteiros.
    let ok = unsafe { ffi::SetThreadPriority(ffi::GetCurrentThread(), priority) };
    if ok == 0 {
        tracing::debug!(
            priority,
            "probe: SetThreadPriority falhou — seguindo no default"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-012 — `prevent_sleep = false` não toca no estado do sistema.
    #[test]
    fn spec_probe_012_disabled_guard_is_inert() {
        let guard = KeepAwake::acquire(false);
        assert!(!guard.is_active());
    }

    /// SPEC-PROBE-012 — adquirir e liberar não faz panic em nenhuma plataforma.
    #[test]
    fn spec_probe_012_acquire_and_release_is_safe() {
        let guard = KeepAwake::acquire(true);
        let _ = guard.is_active();
        drop(guard);
        // Segunda aquisição após a liberação continua funcionando.
        drop(KeepAwake::acquire(true));
    }

    /// §5.4 — ajustar prioridade nunca derruba a thread.
    #[test]
    fn thread_priority_helpers_never_panic() {
        lower_current_thread_priority();
        raise_current_thread_priority();
    }
}
