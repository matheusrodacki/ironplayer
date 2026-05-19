use thiserror::Error;

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
/// SPEC-NET-002
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// Timeout sem pacotes recebidos.
    Timeout,
    /// Recepção iniciada com sucesso.
    Started,
    /// Recepção encerrada normalmente.
    Stopped,
}

/// Eventos emitidos pelo `RtpStripper`.
///
/// SPEC-NET-003
#[derive(Debug, Clone)]
pub enum RtpEvent {
    /// Pacote fora de ordem detectado.
    OutOfOrder { expected: u16, got: u16 },
}
