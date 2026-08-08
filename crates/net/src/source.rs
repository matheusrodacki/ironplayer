//! Backend de aquisição de datagramas.
//!
//! Toda a camada de análise da spec-14 fala com um [`PacketSource`], nunca com
//! um socket: trocar o backend (socket UDP hoje, captura pcap/Npcap depois) não
//! pode alterar o motor de análise.  O que muda entre backends é
//! [`SourceCapabilities`] — e um check que o backend ativo não observa vira
//! `n/a`, nunca verde (SPEC-PROBE-IP-002).
//!
//! SPEC-PROBE-IP-001 · SPEC-PROBE-IP-002 · SPEC-PROBE-IP-003 ·
//! SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009 · SPEC-PROBE-IP-013 ·
//! SPEC-PROBE-IP-051

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use bytes::Bytes;
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, info, warn};

use crate::error::NetError;

/// Um datagrama entregue pelo backend.
///
/// SPEC-PROBE-IP-004 — `at` é carimbado **imediatamente** após o `recv_from`,
/// antes de qualquer parsing ou envio para canal.
#[derive(Debug, Clone)]
pub struct Datagram {
    pub data: Bytes,
    /// Endereço de origem — o `recv` antigo descartava isto, e sem ele não há
    /// como detectar duas fontes no mesmo grupo (SPEC-PROBE-IP-010).
    pub from: SocketAddrV4,
    pub at: Instant,
}

/// O que o backend ativo consegue observar.
///
/// SPEC-PROBE-IP-002 — um socket UDP **não** entrega o cabeçalho IP; os checks
/// que dependem dele (DF bit, TTL, TOS/DSCP, MTU do enlace) são marcados "não
/// aplicável" em vez de reportados como conformes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// Cabeçalho IP visível: DF bit, TTL, TOS/DSCP.
    pub ip_header: bool,
    /// Fragmentação IP e MTU do enlace observáveis.
    pub link_mtu: bool,
}

impl SourceCapabilities {
    /// Socket UDP: o kernel remonta fragmentos e esconde o cabeçalho IP.
    pub const SOCKET: Self = Self {
        ip_header: false,
        link_mtu: false,
    };

    /// Captura na NIC (fase 4 do faseamento, atrás da feature `capture-pcap`).
    pub const CAPTURE: Self = Self {
        ip_header: true,
        link_mtu: true,
    };

    /// Rótulo estável para o `session.toml` e para o tooltip da UI.
    pub fn label(self) -> &'static str {
        if self.ip_header {
            "capture"
        } else {
            "socket"
        }
    }
}

/// Parâmetros **efetivos** do backend, depois de o kernel opinar.
///
/// SPEC-PROBE-IP-013 — `SO_RCVBUF` é o valor real, não o solicitado.
/// SPEC-PROBE-IP-051 — a interface é a que o join realmente usou; foi a falta
/// dela que custou uma sessão inteira de investigação em 07/08/2026.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceBinding {
    pub group: Ipv4Addr,
    pub port: u16,
    /// Interface efetiva do join; `0.0.0.0` = escolhida pela tabela de rotas.
    pub iface: Ipv4Addr,
    pub source: Option<Ipv4Addr>,
    pub so_rcvbuf_bytes: usize,
}

impl SourceBinding {
    /// Interface como texto, com `default` no lugar de `0.0.0.0`.
    ///
    /// SPEC-PROBE-IP-051
    pub fn iface_label(&self) -> String {
        if self.iface.is_unspecified() {
            "default".to_string()
        } else {
            self.iface.to_string()
        }
    }
}

/// Fonte de datagramas.
///
/// SPEC-PROBE-IP-001 — o motor de análise depende só deste contrato.
pub trait PacketSource: Send {
    /// Recebe o próximo datagrama em `buf`.
    ///
    /// `Ok(None)` é **timeout**, não erro: um feed sem tráfego é um diagnóstico
    /// válido e não pode derrubar a sessão.
    fn recv(&mut self, buf: &mut [u8]) -> Result<Option<Datagram>, NetError>;

    /// O que este backend observa (SPEC-PROBE-IP-002).
    fn capabilities(&self) -> SourceCapabilities;

    /// Parâmetros efetivos do bind/join.
    fn binding(&self) -> SourceBinding;
}

/// Backend padrão: socket UDP multicast.
///
/// SPEC-PROBE-IP-001
pub struct SocketSource {
    socket: Option<std::net::UdpSocket>,
    binding: SourceBinding,
}

/// Configuração do socket.
#[derive(Debug, Clone, Copy)]
pub struct SocketSourceConfig {
    /// `SO_RCVBUF` solicitado, em bytes.
    pub buf_size: usize,
    /// Timeout de cada `recv_from`.
    pub timeout: Duration,
}

impl Default for SocketSourceConfig {
    fn default() -> Self {
        Self {
            buf_size: 4_194_304,
            timeout: Duration::from_millis(5_000),
        }
    }
}

impl SocketSource {
    /// Cria o socket, faz bind e entra no grupo multicast.
    ///
    /// SPEC-PROBE-IP-012 — o ciclo de join é observável: sucesso vira `info!`
    /// com a interface efetiva, falha vira [`NetError::JoinFailed`].
    pub fn join(
        group: Ipv4Addr,
        port: u16,
        iface: Option<Ipv4Addr>,
        source: Option<Ipv4Addr>,
        cfg: SocketSourceConfig,
    ) -> Result<Self, NetError> {
        let iface_addr = iface.unwrap_or(Ipv4Addr::UNSPECIFIED);

        let socket =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(NetError::Io)?;
        socket
            .set_recv_buffer_size(cfg.buf_size)
            .map_err(NetError::Io)?;

        // SPEC-PROBE-IP-013 — o kernel trunca `SO_RCVBUF` silenciosamente; o
        // que vale no diagnóstico é o valor efetivo, e ele vai para o
        // `session.toml`.
        let effective = match socket.recv_buffer_size() {
            Ok(actual) => {
                if actual < cfg.buf_size {
                    warn!(
                        requested = cfg.buf_size,
                        actual, "SO_RCVBUF truncado pelo kernel"
                    );
                } else {
                    debug!(buf_size = actual, "SO_RCVBUF configurado");
                }
                actual
            }
            Err(e) => {
                warn!(error = %e, "não foi possível verificar SO_RCVBUF");
                0
            }
        };

        socket.set_reuse_address(true).map_err(NetError::Io)?;
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
        socket.bind(&bind_addr.into()).map_err(NetError::Io)?;

        if let Some(src) = source {
            socket
                .join_ssm_v4(&src, &group, &iface_addr)
                .map_err(NetError::JoinFailed)?;
            info!(source = %src, group = %group, port, iface = %iface_addr, "SSM multicast join OK");
        } else {
            socket
                .join_multicast_v4(&group, &iface_addr)
                .map_err(NetError::JoinFailed)?;
            info!(group = %group, port, iface = %iface_addr, "multicast join OK");
        }

        socket
            .set_read_timeout(Some(cfg.timeout))
            .map_err(NetError::Io)?;

        Ok(Self {
            socket: Some(socket.into()),
            binding: SourceBinding {
                group,
                port,
                iface: iface_addr,
                source,
                so_rcvbuf_bytes: effective,
            },
        })
    }

    /// Sai do grupo e fecha o socket.
    ///
    /// SPEC-PROBE-IP-012 — `leave` é uma transição registrada, não um efeito
    /// colateral silencioso do `Drop`.
    pub fn leave(&mut self) {
        let Some(sock) = self.socket.take() else {
            return;
        };
        let sock2 = Socket::from(sock);
        let result = match self.binding.source {
            Some(src) => sock2.leave_ssm_v4(&src, &self.binding.group, &self.binding.iface),
            None => sock2.leave_multicast_v4(&self.binding.group, &self.binding.iface),
        };
        match result {
            Ok(()) => info!(group = %self.binding.group, port = self.binding.port, "multicast leave OK"),
            Err(e) => warn!(error = %e, group = %self.binding.group, "falha ao sair do grupo multicast"),
        }
    }
}

impl Drop for SocketSource {
    fn drop(&mut self) {
        self.leave();
    }
}

impl PacketSource for SocketSource {
    fn recv(&mut self, buf: &mut [u8]) -> Result<Option<Datagram>, NetError> {
        let Some(sock) = self.socket.as_ref() else {
            return Ok(None);
        };
        match sock.recv_from(buf) {
            Ok((n, from)) => {
                // SPEC-PROBE-IP-004 — carimba antes de copiar, antes de
                // parsear, antes de enviar para canal.
                let at = Instant::now();
                let from = match from {
                    SocketAddr::V4(v4) => v4,
                    // Socket IPv4: a stdlib só devolve V6 se o socket for
                    // dual-stack, o que não é o caso aqui.  Ainda assim, não
                    // vale um panic sobre dado de rede (RNF-PRB-003).
                    SocketAddr::V6(_) => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
                };
                Ok(Some(Datagram {
                    data: Bytes::copy_from_slice(&buf[..n]),
                    from,
                    at,
                }))
            }
            Err(e) if is_timeout(&e) => Ok(None),
            Err(e) if is_interrupted(&e) => Ok(None),
            Err(e) => Err(NetError::Io(e)),
        }
    }

    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities::SOCKET
    }

    fn binding(&self) -> SourceBinding {
        self.binding
    }
}

/// Backend de captura na NIC — **fase 4** do faseamento (§9).
///
/// SPEC-PROBE-IP-003 — a feature existe para que a fronteira do
/// [`PacketSource`] seja real e verificável pelo compilador, e fica desligada
/// por default: sem ela, `cargo build` não exige Npcap instalado.  A
/// implementação só entra se a investigação exigir a camada IP abaixo do UDP
/// (DF bit, MTU, TTL) — hoje o que dói é perda, reordenação e FEC.
#[cfg(feature = "capture-pcap")]
pub mod capture {
    use super::{Datagram, NetError, PacketSource, SourceBinding, SourceCapabilities};

    /// Captura pcap/Npcap.
    pub struct CaptureSource {
        binding: SourceBinding,
    }

    impl CaptureSource {
        /// Ainda não implementado — devolve erro em vez de fingir que mede.
        pub fn open(_binding: SourceBinding) -> Result<Self, NetError> {
            Err(NetError::MalformedUrl(
                "backend de captura pcap é fase 4 e ainda não foi implementado".to_string(),
            ))
        }
    }

    impl PacketSource for CaptureSource {
        fn recv(&mut self, _buf: &mut [u8]) -> Result<Option<Datagram>, NetError> {
            Ok(None)
        }

        fn capabilities(&self) -> SourceCapabilities {
            SourceCapabilities::CAPTURE
        }

        fn binding(&self) -> SourceBinding {
            self.binding
        }
    }
}

/// Retorna `true` se o erro é um timeout de I/O.
pub(crate) fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Retorna `true` se o erro é uma interrupção (EINTR).
pub(crate) fn is_interrupted(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::Interrupted
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-IP-002 — o backend de socket declara que **não** observa a
    /// camada IP; é isso que faz DF bit e MTU virarem `n/a` em vez de verde.
    #[test]
    fn spec_probe_ip_002_socket_backend_does_not_observe_ip_header() {
        // Comparação com o valor inteiro, e não `assert!(!…ip_header)`: um
        // campo `const` faz a asserção virar constante e o clippy, com razão,
        // recusa um teste que não pode falhar em runtime.
        assert_eq!(
            SourceCapabilities::SOCKET,
            SourceCapabilities {
                ip_header: false,
                link_mtu: false
            }
        );
        assert_eq!(
            SourceCapabilities::CAPTURE,
            SourceCapabilities {
                ip_header: true,
                link_mtu: true
            }
        );
        assert_ne!(SourceCapabilities::SOCKET, SourceCapabilities::CAPTURE);
        assert_eq!(SourceCapabilities::SOCKET.label(), "socket");
        assert_eq!(SourceCapabilities::CAPTURE.label(), "capture");
    }

    /// SPEC-PROBE-IP-051 — a interface efetiva é legível no `session.toml`,
    /// com `default` no lugar do `0.0.0.0` que ninguém consegue interpretar.
    #[test]
    fn spec_probe_ip_051_binding_reports_effective_interface() {
        let b = SourceBinding {
            group: "239.15.0.183".parse().expect("grupo"),
            port: 50_000,
            iface: Ipv4Addr::UNSPECIFIED,
            source: None,
            so_rcvbuf_bytes: 4_194_304,
        };
        assert_eq!(b.iface_label(), "default");

        let fixed = SourceBinding {
            iface: "10.0.0.7".parse().expect("iface"),
            ..b
        };
        assert_eq!(fixed.iface_label(), "10.0.0.7");
    }

    /// SPEC-PROBE-IP-013 — o bind devolve o `SO_RCVBUF` efetivo, não o pedido.
    ///
    /// Roda em `127.0.0.1` para não depender de rede real; se o ambiente não
    /// suportar multicast (CI restrito), o teste apenas não afirma nada.
    #[test]
    fn spec_probe_ip_013_join_reports_effective_rcvbuf() {
        let cfg = SocketSourceConfig {
            buf_size: 262_144,
            timeout: Duration::from_millis(50),
        };
        let src = SocketSource::join(
            "239.255.20.1".parse().expect("grupo"),
            56_100,
            Some("127.0.0.1".parse().expect("iface")),
            None,
            cfg,
        );
        let Ok(src) = src else {
            return; // ambiente sem multicast — nada a afirmar
        };
        let b = src.binding();
        assert_eq!(b.port, 56_100);
        assert_eq!(b.iface_label(), "127.0.0.1");
        assert!(b.so_rcvbuf_bytes > 0, "o valor efetivo precisa ser lido");
    }
}
