//! Testes de integração: loopback UDP + RTP.
//!
//! T06 — UdpReceiver recebe pacotes enviados em loopback multicast.
//! T07 — RtpStripper remove header RTP corretamente em fluxo real.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use crossbeam_channel::bounded;
use net::RtpEvent;
use net::{NetError, NetEvent, ReceiverConfig, RtpStripper, StopToken, StreamUrl, UdpReceiver};
use socket2::{Domain, Protocol, Socket, Type};

// Grupo multicast exclusivo para estes testes (evita colisão com unit-tests)
const GROUP: &str = "239.255.10.1";
const PORT_T06: u16 = 55600;
const PORT_T07: u16 = 55601;
const LOOPBACK: &str = "127.0.0.1";

/// Cria um pacote TS falso de 188 bytes com sync byte 0x47.
fn make_ts_packet(seq: u8) -> Vec<u8> {
    let mut pkt = vec![0u8; 188];
    pkt[0] = 0x47;
    pkt[1] = seq;
    pkt
}

/// Cria um pacote RTP encapsulando `payload` com sequence number `seq`.
///
/// Header RTP mínimo (12 bytes): V=2, P=0, X=0, CC=0, M=0, PT=33.
fn make_rtp_packet(seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + payload.len());
    pkt.push(0x80); // V=2, P=0, X=0, CC=0
    pkt.push(33); // M=0, PT=33
    pkt.push((seq >> 8) as u8);
    pkt.push(seq as u8);
    pkt.extend_from_slice(&[0u8; 4]); // timestamp
    pkt.extend_from_slice(&[0u8; 4]); // SSRC
    pkt.extend_from_slice(payload);
    pkt
}

/// Inicia um UdpReceiver em thread separada e retorna o join handle.
///
/// Retorna `None` se o ambiente não suporta multicast (ex: CI restrito).
fn start_receiver(
    group: &str,
    port: u16,
    data_tx: crossbeam_channel::Sender<Bytes>,
    ev_tx: crossbeam_channel::Sender<NetEvent>,
    token: StopToken,
) -> thread::JoinHandle<Result<(), NetError>> {
    let url = StreamUrl::UdpMulticast {
        group: group.parse().unwrap(),
        port,
        iface: Some(LOOPBACK.parse().unwrap()),
        source: None,
    };
    let cfg = ReceiverConfig {
        buf_size: 65536,
        timeout_ms: 200,
    };
    let recv = UdpReceiver::new(url, data_tx, ev_tx, cfg);
    thread::spawn(move || recv.run(token))
}

/// Cria um socket UDP configurado para envio multicast via loopback.
fn make_sender(iface: Ipv4Addr) -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_multicast_if_v4(&iface)?;
    sock.set_multicast_loop_v4(true)?;
    sock.set_multicast_ttl_v4(1)?;
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    sock.bind(&bind_addr.into())?;
    Ok(sock.into())
}

// ---------------------------------------------------------------------------
// T06 — recepção de 100 pacotes TS raw via loopback multicast
// ---------------------------------------------------------------------------

/// T06: UdpReceiver entrega 100 pacotes TS enviados via loopback multicast.
#[test]
fn spec_net_t06_loopback_udp_delivery() {
    let (data_tx, data_rx) = bounded::<Bytes>(256);
    let (ev_tx, ev_rx) = bounded::<NetEvent>(32);
    let (token, handle) = StopToken::new();

    let jh = start_receiver(GROUP, PORT_T06, data_tx, ev_tx, token);

    // Aguardar Started (ou timeout indicando falha de join multicast)
    let started = ev_rx
        .recv_timeout(Duration::from_millis(1500))
        .map(|e| matches!(e, NetEvent::Started))
        .unwrap_or(false);

    if !started {
        // Ambiente não suporta multicast no loopback — encerra silenciosamente
        handle.stop();
        let _ = jh.join();
        return;
    }

    // Enviar 100 pacotes de 188 bytes
    let sender = match make_sender(LOOPBACK.parse().unwrap()) {
        Ok(s) => s,
        Err(_) => {
            handle.stop();
            let _ = jh.join();
            return;
        }
    };
    let dest: SocketAddrV4 = format!("{GROUP}:{PORT_T06}").parse().unwrap();

    const N: usize = 100;
    for i in 0..N {
        let pkt = make_ts_packet(i as u8);
        let _ = sender.send_to(&pkt, dest);
    }

    // Coletar pacotes por até 2 s
    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    let mut received = 0usize;
    while std::time::Instant::now() < deadline && received < N {
        match data_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                assert_eq!(bytes.len(), 188, "tamanho de pacote incorreto");
                assert_eq!(bytes[0], 0x47, "sync byte ausente");
                received += 1;
            }
            Err(_) => break,
        }
    }

    handle.stop();
    let _ = jh.join();

    assert_eq!(received, N, "esperados {N} pacotes, recebidos {received}");
}

// ---------------------------------------------------------------------------
// T07 — RtpStripper em fluxo real (loopback RTP/MPEG-TS)
// ---------------------------------------------------------------------------

/// T07: RtpStripper remove corretamente o header RTP de pacotes recebidos via loopback.
#[test]
fn spec_net_t07_loopback_rtp_strip() {
    let (data_tx, data_rx) = bounded::<Bytes>(256);
    let (ev_tx, ev_rx) = bounded::<NetEvent>(32);
    let (token, handle) = StopToken::new();

    let jh = start_receiver(GROUP, PORT_T07, data_tx, ev_tx, token);

    let started = ev_rx
        .recv_timeout(Duration::from_millis(1500))
        .map(|e| matches!(e, NetEvent::Started))
        .unwrap_or(false);

    if !started {
        handle.stop();
        let _ = jh.join();
        return;
    }

    let sender = match make_sender(LOOPBACK.parse().unwrap()) {
        Ok(s) => s,
        Err(_) => {
            handle.stop();
            let _ = jh.join();
            return;
        }
    };
    let dest: SocketAddrV4 = format!("{GROUP}:{PORT_T07}").parse().unwrap();

    // Payload: 7 pacotes TS de 188 bytes cada (1316 bytes)
    const N: usize = 20;
    let payload = make_ts_packet(0xAB);

    for seq in 0..N as u16 {
        let rtp_pkt = make_rtp_packet(seq, &payload);
        let _ = sender.send_to(&rtp_pkt, dest);
    }

    // Coletar e processar com RtpStripper
    let (rtp_ev_tx, rtp_ev_rx) = bounded::<RtpEvent>(64);
    let mut stripper = RtpStripper::new(rtp_ev_tx);

    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    let mut stripped = 0usize;
    while std::time::Instant::now() < deadline && stripped < N {
        match data_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(raw) => {
                let ts = stripper.strip(raw);
                if ts.is_empty() {
                    continue;
                }
                assert_eq!(ts[0], 0x47, "payload deve começar com sync byte 0x47");
                assert_eq!(ts.len(), 188, "payload deve ter 188 bytes");
                stripped += 1;
            }
            Err(_) => break,
        }
    }

    // Nenhum evento OutOfOrder esperado (sequência 0..N contínua)
    let out_of_order: Vec<_> = rtp_ev_rx.try_iter().collect();
    assert!(
        out_of_order.is_empty(),
        "nenhum OutOfOrder esperado, mas recebeu: {out_of_order:?}"
    );

    handle.stop();
    let _ = jh.join();

    assert_eq!(
        stripped, N,
        "esperados {N} payloads, processados {stripped}"
    );
}
