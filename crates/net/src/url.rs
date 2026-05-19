/// SPEC-NET-001
///
/// Parse de URLs de stream UDP/RTP multicast.
use std::net::Ipv4Addr;

use crate::error::NetError;

/// Representa uma URL de stream multicast.
///
/// SPEC-NET-001
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamUrl {
    /// `udp://@<group>:<port>[?iface=<ip>]`
    UdpMulticast {
        group: Ipv4Addr,
        port: u16,
        iface: Option<Ipv4Addr>,
    },
    /// `rtp://@<group>:<port>[?iface=<ip>]`
    RtpMulticast {
        group: Ipv4Addr,
        port: u16,
        iface: Option<Ipv4Addr>,
    },
}

impl StreamUrl {
    /// Faz o parse de uma URL de stream multicast.
    ///
    /// SPEC-NET-001
    pub fn parse(url: &str) -> Result<Self, NetError> {
        // Determina o esquema
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| NetError::MalformedUrl(url.to_string()))?;

        let scheme = scheme.to_lowercase();
        if scheme != "udp" && scheme != "rtp" {
            return Err(NetError::UnknownScheme(scheme));
        }

        // Remove '@' opcional no início do host
        let rest = rest.strip_prefix('@').unwrap_or(rest);

        // Separa query string
        let (host_port, query) = match rest.split_once('?') {
            Some((h, q)) => (h, Some(q)),
            None => (rest, None),
        };

        // Separa host e porta
        let (host_str, port_str) = host_port
            .rsplit_once(':')
            .ok_or_else(|| NetError::MalformedUrl(url.to_string()))?;

        let group: Ipv4Addr = host_str
            .parse()
            .map_err(|_| NetError::MalformedUrl(format!("endereço inválido: {host_str}")))?;

        let port: u16 = port_str
            .parse()
            .map_err(|_| NetError::MalformedUrl(format!("porta inválida: {port_str}")))?;

        // Valida multicast (224.0.0.0/4): primeiro octeto em [224, 239]
        if group.octets()[0].wrapping_sub(224) >= 16 {
            return Err(NetError::NotMulticast(group));
        }

        // Valida porta não-zero
        if port == 0 {
            return Err(NetError::InvalidPort);
        }

        // Parse do parâmetro ?iface=
        let iface = if let Some(q) = query {
            parse_iface(q, url)?
        } else {
            None
        };

        if scheme == "udp" {
            Ok(StreamUrl::UdpMulticast { group, port, iface })
        } else {
            Ok(StreamUrl::RtpMulticast { group, port, iface })
        }
    }
}

fn parse_iface(query: &str, url: &str) -> Result<Option<Ipv4Addr>, NetError> {
    for param in query.split('&') {
        if let Some(val) = param.strip_prefix("iface=") {
            let addr: Ipv4Addr = val
                .parse()
                .map_err(|_| NetError::MalformedUrl(format!("iface inválido: {val} em {url}")))?;
            return Ok(Some(addr));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-NET-001 — caso udp multicast válido sem iface
    #[test]
    fn spec_net_001_udp_multicast_valid() {
        let result = StreamUrl::parse("udp://@239.1.1.1:1234").unwrap();
        assert_eq!(
            result,
            StreamUrl::UdpMulticast {
                group: "239.1.1.1".parse().unwrap(),
                port: 1234,
                iface: None,
            }
        );
    }

    /// SPEC-NET-001 — caso rtp multicast válido sem iface
    #[test]
    fn spec_net_001_rtp_multicast_valid() {
        let result = StreamUrl::parse("rtp://@239.0.0.5:5004").unwrap();
        assert_eq!(
            result,
            StreamUrl::RtpMulticast {
                group: "239.0.0.5".parse().unwrap(),
                port: 5004,
                iface: None,
            }
        );
    }

    /// SPEC-NET-001 — endereço não-multicast retorna NotMulticast
    #[test]
    fn spec_net_001_not_multicast() {
        let result = StreamUrl::parse("udp://10.0.0.1:1234");
        assert!(matches!(result, Err(NetError::NotMulticast(_))));
    }

    /// SPEC-NET-001 — porta zero retorna InvalidPort
    #[test]
    fn spec_net_001_invalid_port() {
        let result = StreamUrl::parse("udp://@239.1.1.1:0");
        assert!(matches!(result, Err(NetError::InvalidPort)));
    }

    /// SPEC-NET-001 — esquema desconhecido retorna UnknownScheme
    #[test]
    fn spec_net_001_unknown_scheme() {
        let result = StreamUrl::parse("http://example.com");
        assert!(matches!(result, Err(NetError::UnknownScheme(_))));
    }

    /// SPEC-NET-001 — parsing de ?iface=
    #[test]
    fn spec_net_001_iface_param() {
        let result = StreamUrl::parse("udp://@239.1.1.1:1234?iface=192.168.1.10").unwrap();
        assert_eq!(
            result,
            StreamUrl::UdpMulticast {
                group: "239.1.1.1".parse().unwrap(),
                port: 1234,
                iface: Some("192.168.1.10".parse().unwrap()),
            }
        );
    }
}
