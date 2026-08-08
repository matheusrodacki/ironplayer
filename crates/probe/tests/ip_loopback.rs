//! Camada 1 sobre loopback multicast real.
//!
//! Os testes unitários do [`probe::IpAnalyzer`] injetam `Datagram` construídos
//! à mão; estes aqui fecham o circuito pelo socket, que é onde moram as coisas
//! que uma fixture não pega: `recv_from` devolvendo o endereço de origem, o
//! carimbo de tempo do retorno do `recv`, e os grupos de FEC em `base+2`/`base+4`.
//!
//! Padrão do [`crates/net/tests/net_loopback.rs`]: se o ambiente não suportar
//! multicast em loopback (CI restrito), o teste encerra sem afirmar nada em vez
//! de falhar por um motivo que não é o código.
//!
//! SPEC-PROBE-IP-004 · SPEC-PROBE-IP-009 · SPEC-PROBE-IP-030 …
//! SPEC-PROBE-IP-036 · SPEC-PROBE-IP-051

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use net::{
    PacketSource, SocketSource, SocketSourceConfig, FEC_HEADER_LEN, RTP_PT_MPEGTS, TS_PACKET_LEN,
    TS_SYNC_BYTE,
};
use probe::{Encapsulation, FecMode, IpAnalyzer, IpAnalyzerConfig, ProbeConfig};
use socket2::{Domain, Protocol, Socket, Type};

const GROUP: &str = "239.255.30.7";
const LOOPBACK: &str = "127.0.0.1";
/// Porta base do feed; a FEC vive em `+2` e `+4`, como na operação.
const BASE_PORT: u16 = 56_400;

fn loopback() -> Ipv4Addr {
    LOOPBACK.parse().expect("loopback")
}

fn group() -> Ipv4Addr {
    GROUP.parse().expect("grupo")
}

fn socket_cfg() -> SocketSourceConfig {
    SocketSourceConfig {
        buf_size: 262_144,
        timeout: Duration::from_millis(150),
    }
}

/// Emissor multicast em loopback.
fn sender() -> Option<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).ok()?;
    sock.set_multicast_if_v4(&loopback()).ok()?;
    sock.set_multicast_loop_v4(true).ok()?;
    sock.set_multicast_ttl_v4(1).ok()?;
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into())
        .ok()?;
    Some(sock.into())
}

/// Datagrama RTP com 7 pacotes TS.
fn rtp_datagram(seq: u16, ssrc: u32) -> Vec<u8> {
    let mut pkt = vec![0x80, RTP_PT_MPEGTS];
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&(u32::from(seq) * 3_600).to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    let mut payload = vec![0u8; 7 * TS_PACKET_LEN];
    for chunk in payload.chunks_exact_mut(TS_PACKET_LEN) {
        chunk[0] = TS_SYNC_BYTE;
    }
    pkt.extend_from_slice(&payload);
    pkt
}

/// Datagrama de FEC: `d = false` é coluna, `true` é linha.
fn fec_datagram(d: bool, offset: u8, na: u8, ssrc: u32) -> Vec<u8> {
    let mut pkt = vec![0x80, 96, 0x00, 0x01];
    pkt.extend_from_slice(&0u32.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    let mut fec = [0u8; FEC_HEADER_LEN];
    fec[12] = if d { 0x40 } else { 0x00 };
    fec[13] = offset;
    fec[14] = na;
    pkt.extend_from_slice(&fec);
    pkt.extend_from_slice(&[0u8; 1_316]);
    pkt
}

fn analyzer(fec: FecMode) -> IpAnalyzer {
    IpAnalyzer::new(IpAnalyzerConfig::from_config(
        &ProbeConfig::default(),
        fec,
        Encapsulation::Rtp,
    ))
}

/// SPEC-PROBE-IP-009 — `recv_from` devolve o endereço de origem, e o instante
/// de chegada é carimbado no retorno do `recv` (SPEC-PROBE-IP-004).
///
/// O `recv` antigo descartava a origem; sem ela não há como detectar duas
/// fontes no mesmo grupo, que é um dos diagnósticos da camada.
#[test]
fn spec_probe_ip_009_recv_from_reports_source_and_arrival_time() {
    let port = BASE_PORT;
    let Ok(mut source) = SocketSource::join(group(), port, Some(loopback()), None, socket_cfg())
    else {
        return; // ambiente sem multicast
    };
    let Some(tx) = sender() else { return };
    let dest = SocketAddrV4::new(group(), port);

    let before = Instant::now();
    for seq in 0..20u16 {
        let _ = tx.send_to(&rtp_datagram(seq, 0xABCD), dest);
    }

    let mut buf = vec![0u8; 65_536];
    let mut seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && seen < 20 {
        match source.recv(&mut buf) {
            Ok(Some(datagram)) => {
                assert_eq!(
                    datagram.from.ip(),
                    &loopback(),
                    "o endereço de origem precisa chegar ao analisador"
                );
                assert!(datagram.at >= before, "o carimbo é do instante da chegada");
                assert_eq!(datagram.data.len(), 12 + 7 * TS_PACKET_LEN);
                seen += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("erro de recepção: {e}"),
        }
    }

    if seen == 0 {
        return; // loopback multicast bloqueado no host
    }
    assert_eq!(seen, 20, "todos os datagramas enviados foram entregues");

    // SPEC-PROBE-IP-013 · SPEC-PROBE-IP-051 — os parâmetros efetivos do join.
    let binding = source.binding();
    assert_eq!(binding.port, port);
    assert_eq!(binding.iface_label(), LOOPBACK);
    assert!(binding.so_rcvbuf_bytes > 0);
}

/// SPEC-PROBE-IP-030 · SPEC-PROBE-IP-032 — FEC em 50002/50004 para um feed em
/// 50000: a matriz sai completa das **duas** portas, e o encapsulamento vira
/// `RTP+FEC`.
///
/// SPEC-PROBE-IP-051 — os três joins acontecem na mesma interface; a FEC
/// aparecendo como ausente por rota divergente seria um falso negativo caro.
#[test]
fn spec_probe_ip_030_fec_on_base_plus_two_and_four_completes_the_matrix() {
    let base = BASE_PORT + 10;
    let cfg = socket_cfg();
    let iface = Some(loopback());

    let (Ok(mut main), Ok(mut column), Ok(mut row)) = (
        SocketSource::join(group(), base, iface, None, cfg),
        SocketSource::join(group(), base + 2, iface, None, cfg),
        SocketSource::join(group(), base + 4, iface, None, cfg),
    ) else {
        return; // ambiente sem multicast
    };
    // Os três joins na mesma interface — é isto que SPEC-PROBE-IP-030b exige.
    assert_eq!(main.binding().iface, column.binding().iface);
    assert_eq!(main.binding().iface, row.binding().iface);

    let Some(tx) = sender() else { return };
    const SSRC: u32 = 0x1A2B_3C4D;
    for seq in 0..40u16 {
        let _ = tx.send_to(&rtp_datagram(seq, SSRC), SocketAddrV4::new(group(), base));
    }
    for _ in 0..4 {
        let _ = tx.send_to(
            &fec_datagram(false, 8, 5, SSRC),
            SocketAddrV4::new(group(), base + 2),
        );
        let _ = tx.send_to(
            &fec_datagram(true, 1, 8, SSRC),
            SocketAddrV4::new(group(), base + 4),
        );
    }

    let mut a = analyzer(FecMode::Auto);
    let mut buf = vec![0u8; 65_536];
    let mut main_seen = 0usize;
    let mut fec_seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && (main_seen < 40 || fec_seen < 8) {
        if let Ok(Some(d)) = main.recv(&mut buf) {
            a.on_datagram(&d);
            main_seen += 1;
            continue;
        }
        if let Ok(Some(d)) = column.recv(&mut buf) {
            a.on_fec_datagram(&d);
            fec_seen += 1;
            continue;
        }
        if let Ok(Some(d)) = row.recv(&mut buf) {
            a.on_fec_datagram(&d);
            fec_seen += 1;
        }
    }

    if main_seen == 0 || fec_seen == 0 {
        return; // loopback multicast bloqueado no host
    }

    // O tick precisa vir depois da janela de detecção de encapsulamento.
    std::thread::sleep(Duration::from_millis(50));
    let tick = a.take_tick(Instant::now() + Duration::from_secs(4), 15_000.0);

    assert_eq!(tick.encapsulation, Encapsulation::RtpFec);
    assert!(tick.fec.present);
    assert_eq!(tick.fec.l, Some(8), "L vem do offset do fluxo de coluna");
    assert_eq!(tick.fec.d, Some(5), "D vem do NA do fluxo de coluna");
    assert_eq!(tick.fec.lxd(), Some(40), "dentro do teto de 100 do perfil");
    assert_eq!(tick.fec.streams, 2, "coluna e linha");
    assert!(!tick.fec.ssrc_mismatch, "a FEC protege o mesmo SSRC");
}

/// SPEC-PROBE-IP-030 · SPEC-PROBE-IP-030a — feed sem FEC com `fec = auto`:
/// `fec_present = false`, nenhum alarme, e a recepção principal intacta.
#[test]
fn spec_probe_ip_030a_feed_without_fec_keeps_the_main_reception() {
    let base = BASE_PORT + 20;
    let cfg = socket_cfg();
    let iface = Some(loopback());

    let (Ok(mut main), Ok(mut column)) = (
        SocketSource::join(group(), base, iface, None, cfg),
        // O grupo de FEC existe como socket, mas ninguém transmite nele.
        SocketSource::join(group(), base + 2, iface, None, cfg),
    ) else {
        return;
    };
    let Some(tx) = sender() else { return };
    for seq in 0..40u16 {
        let _ = tx.send_to(&rtp_datagram(seq, 0x9999), SocketAddrV4::new(group(), base));
    }

    let mut a = analyzer(FecMode::Auto);
    let mut buf = vec![0u8; 65_536];
    let mut seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && seen < 40 {
        match main.recv(&mut buf) {
            Ok(Some(d)) => {
                assert!(
                    a.on_datagram(&d).is_some(),
                    "o payload continua indo para o demux"
                );
                seen += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("erro de recepção: {e}"),
        }
    }
    // Nada chega na porta de FEC — e isso não é erro.
    assert!(matches!(column.recv(&mut buf), Ok(None)));

    if seen == 0 {
        return;
    }
    let tick = a.take_tick(Instant::now() + Duration::from_secs(4), 15_000.0);
    assert!(tick.fec.listening, "com `auto`, a probe escuta");
    assert!(!tick.fec.present, "sem tráfego, `fec_present = false`");
    assert_eq!(tick.fec.l, None);
    assert_eq!(
        tick.encapsulation,
        Encapsulation::Rtp,
        "sem FEC o badge continua RTP, não RTP+FEC"
    );
    assert_eq!(
        tick.rtp.expect("contadores de RTP").received as usize,
        seen,
        "a recepção principal ficou intacta"
    );
}

/// §7 — dois feeds simultâneos, um `RtpFec` e um `Udp`: nenhum contador cruza
/// entre os slots.
///
/// SPEC-PROBE-IP-042 … SPEC-PROBE-IP-046 — todo o estado da camada é por feed.
#[test]
fn spec_probe_ip_046_two_feeds_keep_independent_state() {
    let t0 = Instant::now();
    let mut with_rtp = analyzer(FecMode::Auto);
    let mut pure_udp = IpAnalyzer::new(IpAnalyzerConfig::from_config(
        &ProbeConfig::default(),
        FecMode::Off,
        Encapsulation::Udp,
    ));

    let mut ts_only = vec![0u8; 7 * TS_PACKET_LEN];
    for chunk in ts_only.chunks_exact_mut(TS_PACKET_LEN) {
        chunk[0] = TS_SYNC_BYTE;
    }

    for i in 0..200u64 {
        let at = t0 + Duration::from_millis(i * 20);
        // O feed com RTP perde um pacote a cada 50.
        let seq = (i + i / 50) as u16;
        with_rtp.on_datagram(&net::Datagram {
            data: bytes::Bytes::from(rtp_datagram(seq, 0x5555)),
            from: SocketAddrV4::new(loopback(), 50_000),
            at,
        });
        pure_udp.on_datagram(&net::Datagram {
            data: bytes::Bytes::from(ts_only.clone()),
            from: SocketAddrV4::new(loopback(), 50_010),
            at,
        });
    }
    with_rtp.on_fec_datagram(&net::Datagram {
        data: bytes::Bytes::from(fec_datagram(false, 8, 5, 0x5555)),
        from: SocketAddrV4::new(loopback(), 50_002),
        at: t0,
    });

    let now = t0 + Duration::from_secs(5);
    let rtp_tick = with_rtp.take_tick(now, 15_000.0);
    let udp_tick = pure_udp.take_tick(now, 15_000.0);

    assert_eq!(rtp_tick.encapsulation, Encapsulation::RtpFec);
    assert!(rtp_tick.rtp.expect("rtp").missing > 0, "o feed com RTP mede perda");
    assert!(rtp_tick.fec.present);

    assert_eq!(udp_tick.encapsulation, Encapsulation::Udp);
    assert!(udp_tick.rtp.is_none(), "o feed UDP não herda contador de RTP");
    assert!(!udp_tick.fec.present);
    assert!(!udp_tick.fec.listening, "`fec = off` não escuta nada");
    assert!(udp_tick.violations.is_empty());
    assert_eq!(udp_tick.datagrams, 200);
    assert_eq!(rtp_tick.datagrams, 200);
}
