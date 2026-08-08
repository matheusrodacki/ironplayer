use std::net::SocketAddrV4;

use thiserror::Error;

use crate::source::SourceBinding;

/// Erros da camada de rede.
///
/// SPEC-NET-001, SPEC-NET-002
#[derive(Debug, Error)]
pub enum NetError {
    #[error("endereço não é multicast: {0}")]
    NotMulticast(std::net::Ipv4Addr),

    #[error("porta inválida: 0")]
    InvalidPort,

    #[error("esquema de URL não suportado: {0}")]
    UnsupportedScheme(String),

    #[error("URL malformada: {0}")]
    MalformedUrl(String),

    #[error("falha ao entrar no grupo multicast: {0}")]
    JoinFailed(#[source] std::io::Error),

    #[error("erro de I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Eventos emitidos pelo loop de recepção UDP.
///
/// SPEC-NET-002 · SPEC-PROBE-IP-009 · SPEC-PROBE-IP-012
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// Timeout sem pacotes recebidos.
    Timeout,
    /// Recepção iniciada com sucesso.
    Started,
    /// Recepção encerrada normalmente.
    Stopped,
    /// Entrou no grupo multicast, com os parâmetros **efetivos**.
    ///
    /// SPEC-PROBE-IP-012 · SPEC-PROBE-IP-013 · SPEC-PROBE-IP-051 — cada
    /// transição do ciclo multicast vira evento com timestamp, e a interface
    /// efetiva do join é o que transforma "o multicast sumiu" num diagnóstico
    /// de uma linha em vez de uma sessão inteira procurando regressão.
    Joined(SourceBinding),
    /// Saiu do grupo multicast.
    Left,
    /// Falha de join / bind / interface indisponível.
    JoinFailed { reason: String },
    /// Primeiro datagrama visto de um endereço de origem.
    ///
    /// SPEC-PROBE-IP-009 · SPEC-PROBE-IP-010
    SourceSeen(SocketAddrV4),
}

/// Eventos emitidos pelo `RtpStripper`.
///
/// SPEC-NET-003
#[derive(Debug, Clone)]
pub enum RtpEvent {
    /// Pacote fora de ordem detectado.
    OutOfOrder { expected: u16, got: u16 },
}
