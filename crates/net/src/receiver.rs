//! Loop de recepção UDP multicast.
//!
//! O loop fala com um [`PacketSource`], não com um socket: é isso que permite
//! trocar o backend (captura pcap, fase 4) sem tocar em nada acima
//! (SPEC-PROBE-IP-001).
//!
//! SPEC-NET-002 · SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009 · SPEC-PROBE-IP-012

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use bytes::Bytes;
use crossbeam_channel::Sender;
use tracing::{debug, error, info, warn};

use crate::error::{NetError, NetEvent};
use crate::source::{Datagram, PacketSource, SocketSource, SocketSourceConfig};
use crate::stop::StopToken;
use crate::url::StreamUrl;

/// Configuração do receptor UDP.
///
/// SPEC-NET-002
#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// Tamanho do buffer de kernel (`SO_RCVBUF`). Padrão: 4 MB.
    pub buf_size: usize,
    /// Timeout em milissegundos para cada `recv`. Padrão: 5000 ms.
    pub timeout_ms: u64,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            buf_size: 4_194_304,
            timeout_ms: 5_000,
        }
    }
}

impl ReceiverConfig {
    fn socket_config(&self) -> SocketSourceConfig {
        SocketSourceConfig {
            buf_size: self.buf_size,
            timeout: Duration::from_millis(self.timeout_ms),
        }
    }
}

/// Para onde os datagramas recebidos vão.
///
/// O player só precisa dos bytes; a probe precisa do endereço de origem e do
/// instante de chegada (SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009).  Um `enum`
/// evita duplicar o loop de recepção só por causa do tipo do canal.
enum Sink {
    Raw(Sender<Bytes>),
    Full(Sender<Datagram>),
}

impl Sink {
    /// `false` quando o canal está cheio — o chamador conta o descarte local.
    fn send(&self, datagram: Datagram) -> bool {
        match self {
            Self::Raw(tx) => tx.try_send(datagram.data).is_ok(),
            Self::Full(tx) => tx.try_send(datagram).is_ok(),
        }
    }
}

/// Receptor UDP multicast.
///
/// SPEC-NET-002
pub struct UdpReceiver {
    url: StreamUrl,
    sink: Sink,
    events: Sender<NetEvent>,
    cfg: ReceiverConfig,
}

impl UdpReceiver {
    /// Cria um `UdpReceiver` que entrega apenas os bytes do payload.
    pub fn new(
        url: StreamUrl,
        tx: Sender<Bytes>,
        events: Sender<NetEvent>,
        cfg: ReceiverConfig,
    ) -> Self {
        Self {
            url,
            sink: Sink::Raw(tx),
            events,
            cfg,
        }
    }

    /// Cria um `UdpReceiver` que entrega o datagrama completo.
    ///
    /// SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009 — sem isto não há tempo de chegada
    /// nem IP de origem, e metade dos checks da camada 1 deixa de existir.
    pub fn with_datagrams(
        url: StreamUrl,
        tx: Sender<Datagram>,
        events: Sender<NetEvent>,
        cfg: ReceiverConfig,
    ) -> Self {
        Self {
            url,
            sink: Sink::Full(tx),
            events,
            cfg,
        }
    }

    /// Executa o loop de recepção na thread atual (bloqueante).
    ///
    /// SPEC-NET-002
    pub fn run(self, stop: StopToken) -> Result<(), NetError> {
        let (group, port, iface, source) = match &self.url {
            StreamUrl::UdpMulticast {
                group,
                port,
                iface,
                source,
            }
            | StreamUrl::RtpMulticast {
                group,
                port,
                iface,
                source,
            } => (*group, *port, *iface, *source),
        };

        let mut socket = match SocketSource::join(group, port, iface, source, self.cfg.socket_config())
        {
            Ok(s) => s,
            Err(e) => {
                // SPEC-PROBE-IP-012 — falha de bind/join/interface é uma
                // transição registrada, não só um `Err` que some no log.
                let _ = self.events.try_send(NetEvent::JoinFailed {
                    reason: e.to_string(),
                });
                return Err(e);
            }
        };

        let _ = self.events.try_send(NetEvent::Joined(socket.binding()));
        let _ = self.events.try_send(NetEvent::Started);

        // Buffer de recepção (maior que um pacote TS máximo: 7 × 188 = 1316)
        let mut buf = vec![0u8; 65_536];
        // SPEC-PROBE-IP-010 — duas fontes no mesmo grupo/porta é um diagnóstico,
        // não um detalhe: o conjunto é minúsculo e só cresce quando muda.
        let mut sources: HashSet<SocketAddrV4> = HashSet::new();

        loop {
            if stop.is_stopped() {
                break;
            }

            match socket.recv(&mut buf) {
                Ok(Some(datagram)) => {
                    if sources.insert(datagram.from) {
                        let _ = self.events.try_send(NetEvent::SourceSeen(datagram.from));
                    }
                    // backpressure: descarta se canal cheio
                    if !self.sink.send(datagram) {
                        warn!(group = %group, port, "canal de dados cheio; pacote descartado");
                    }
                }
                Ok(None) => {
                    // SPEC-NET-002c: timeout não é erro fatal
                    debug!("timeout de recepção");
                    let _ = self.events.try_send(NetEvent::Timeout);
                }
                Err(e) => {
                    error!(error = %e, "erro fatal no recv");
                    socket.leave();
                    let _ = self.events.try_send(NetEvent::Left);
                    return Err(e);
                }
            }
        }

        // SPEC-NET-002d — IP_DROP_MEMBERSHIP + fechar socket.
        socket.leave();
        let _ = self.events.try_send(NetEvent::Left);
        let _ = self.events.try_send(NetEvent::Stopped);
        info!(group = %group, "recepção encerrada");
        Ok(())
    }
}

/// Portas de FEC derivadas da porta do feed.
///
/// SPEC-PROBE-IP-030 — a convenção do ST 2022-1 é `base+2` (coluna) e `base+4`
/// (linha); para um feed em 50000, 50002 e 50004.  Devolve `None` quando a soma
/// estouraria o espaço de portas, em vez de dar a volta e entrar num grupo que
/// não tem nada a ver com o feed.
pub fn fec_ports(base: u16, offsets: [u16; 2]) -> Option<(u16, u16)> {
    Some((base.checked_add(offsets[0])?, base.checked_add(offsets[1])?))
}

/// Endereço não especificado, usado quando a origem não é conhecida.
pub const UNSPECIFIED_SOURCE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;
    use std::time::Instant;

    /// SPEC-NET-002: timeout emite NetEvent::Timeout sem panic e sem Err.
    ///
    /// Usa um grupo multicast sem tráfego real; aguarda um timeout e para.
    #[test]
    fn spec_net_002_timeout_no_panic() {
        use std::thread;

        let url = StreamUrl::UdpMulticast {
            group: "239.255.0.1".parse().expect("grupo"),
            port: 54320,
            iface: Some("127.0.0.1".parse().expect("iface")),
            source: None,
        };
        let (tx, _rx) = bounded::<Bytes>(16);
        let (ev_tx, ev_rx) = bounded::<NetEvent>(32);
        let (token, handle) = StopToken::new();
        let cfg = ReceiverConfig {
            buf_size: 65536,
            timeout_ms: 100,
        };
        let recv = UdpReceiver::new(url, tx, ev_tx, cfg);

        let jh = thread::spawn(move || recv.run(token));

        // Aguardar NetEvent::Timeout ou qualquer evento, depois para
        let _ = ev_rx.recv_timeout(Duration::from_millis(1000));
        handle.stop();

        let result = jh.join().expect("thread não deve ter panic");
        // Qualquer resultado (Ok ou Err de rede) é aceitável — o importante é sem panic
        match result {
            Ok(()) | Err(NetError::JoinFailed(_)) | Err(NetError::Io(_)) => {}
            Err(e) => panic!("erro inesperado: {e}"),
        }
    }

    /// SPEC-NET-002: StopToken para o loop sem panic.
    ///
    /// Inicia receptor em thread separada e sinaliza parada antes do timeout.
    #[test]
    fn spec_net_002_stop_token_stops_loop() {
        use std::thread;

        // Porta alta para reduzir conflito; 0.0.0.0 bind pode falhar em CI sem privilégios
        let url = StreamUrl::UdpMulticast {
            group: "239.255.0.2".parse().expect("grupo"),
            port: 54321,
            iface: Some("127.0.0.1".parse().expect("iface")),
            source: None,
        };
        let (tx, _rx) = bounded::<Bytes>(16);
        let (ev_tx, ev_rx) = bounded::<NetEvent>(32);
        let (token, handle) = StopToken::new();
        let cfg = ReceiverConfig {
            buf_size: 65536,
            timeout_ms: 200,
        };
        let recv = UdpReceiver::new(url, tx, ev_tx, cfg);

        let jh = thread::spawn(move || recv.run(token));

        // Aguardar o evento Started ou um timeout (caso join falhe em CI)
        let mut started = false;
        for ev in ev_rx.iter() {
            match ev {
                NetEvent::Started => {
                    started = true;
                    break;
                }
                NetEvent::Timeout | NetEvent::JoinFailed { .. } => {
                    // Se chegou aqui sem Started, provavelmente falhou o join — encerra
                    break;
                }
                _ => {}
            }
        }

        // Sinalizar parada
        handle.stop();

        // A thread deve encerrar sem panic
        let result = jh.join().expect("thread não deve ter panic");
        // Se o join multicast falhou (CI sem suporte), o erro é aceitável
        match result {
            Ok(()) => {}
            Err(NetError::JoinFailed(_)) => {
                // Ambiente sem suporte a multicast — aceitável em CI
                if started {
                    panic!("JoinFailed após Started — inconsistência");
                }
            }
            Err(NetError::Io(_)) => {
                // Bind ou outra falha de I/O — aceitável em CI
            }
            Err(e) => panic!("erro inesperado: {e}"),
        }
    }

    /// SPEC-PROBE-IP-012 — o ciclo multicast é observável: `Joined` carrega a
    /// interface e o `SO_RCVBUF` efetivos, e `Left` fecha o ciclo.
    #[test]
    fn spec_probe_ip_012_multicast_cycle_is_reported() {
        use std::thread;

        let url = StreamUrl::UdpMulticast {
            group: "239.255.0.3".parse().expect("grupo"),
            port: 54322,
            iface: Some("127.0.0.1".parse().expect("iface")),
            source: None,
        };
        let (tx, _rx) = bounded::<Bytes>(16);
        let (ev_tx, ev_rx) = bounded::<NetEvent>(64);
        let (token, handle) = StopToken::new();
        let cfg = ReceiverConfig {
            buf_size: 65536,
            timeout_ms: 100,
        };
        let recv = UdpReceiver::new(url, tx, ev_tx, cfg);
        let jh = thread::spawn(move || recv.run(token));

        let deadline = Instant::now() + Duration::from_millis(1_500);
        let mut binding = None;
        let mut failed = false;
        while Instant::now() < deadline {
            match ev_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(NetEvent::Joined(b)) => {
                    binding = Some(b);
                    break;
                }
                Ok(NetEvent::JoinFailed { .. }) => {
                    failed = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        handle.stop();
        let _ = jh.join();

        if failed {
            return; // ambiente sem multicast — nada a afirmar
        }
        let Some(b) = binding else {
            return;
        };
        assert_eq!(b.port, 54322);
        assert_eq!(b.iface_label(), "127.0.0.1");
        assert!(b.so_rcvbuf_bytes > 0);
        assert!(
            ev_rx.try_iter().any(|e| matches!(e, NetEvent::Left)),
            "o leave precisa fechar o ciclo"
        );
    }

    /// SPEC-PROBE-IP-030 — as portas de FEC saem do próprio feed: 50000 ⇒
    /// 50002 (coluna) e 50004 (linha).
    #[test]
    fn spec_probe_ip_030_fec_ports_derive_from_the_feed_port() {
        assert_eq!(fec_ports(50_000, [2, 4]), Some((50_002, 50_004)));
        // Perto do teto do espaço de portas, não dá a volta.
        assert_eq!(fec_ports(65_534, [2, 4]), None);
    }
}
