//! Token de cancelamento para o loop de recepção UDP.
//!
//! SPEC-NET-002

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Token de cancelamento baseado em `Arc<AtomicBool>`.
///
/// SPEC-NET-002
#[derive(Clone)]
pub struct StopToken(Arc<AtomicBool>);

impl StopToken {
    /// Cria um par `(StopToken, StopHandle)`.
    pub fn new() -> (Self, StopHandle) {
        let flag = Arc::new(AtomicBool::new(false));
        (StopToken(Arc::clone(&flag)), StopHandle(flag))
    }

    /// Retorna `true` se o sinal de parada foi enviado.
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for StopToken {
    fn default() -> Self {
        StopToken(Arc::new(AtomicBool::new(false)))
    }
}

/// Handle que permite sinalizar a parada do receptor.
///
/// SPEC-NET-002
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    /// Sinaliza a parada do loop de recepção.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn spec_net_stop_token_basic() {
        let (token, handle) = StopToken::new();
        assert!(!token.is_stopped(), "deve iniciar como não parado");

        let token_clone = token.clone();
        let t = thread::spawn(move || {
            // outra thread verifica o token
            token_clone.is_stopped()
        });
        assert!(!t.join().unwrap(), "antes do stop: deve ser false em outra thread");

        handle.stop();
        assert!(token.is_stopped(), "após stop: deve ser true");

        let token_clone2 = token.clone();
        let t2 = thread::spawn(move || token_clone2.is_stopped());
        assert!(t2.join().unwrap(), "após stop: deve ser true em outra thread");
    }
}
