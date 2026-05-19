# Spec: `net` — Recepção de Stream

- **Spec-IDs:** SPEC-NET-001 · SPEC-NET-002 · SPEC-NET-003
- **Crate:** `crates/net`
- **Fase:** Alpha v0.1 (NET-001, NET-002) · Alpha v0.2 (NET-003)

---

## Requisitos

| ID            | Requisito                                     | Critério de aceite                                  |
| ------------- | --------------------------------------------- | --------------------------------------------------- |
| SPEC-NET-001  | `StreamUrl` — parse de URLs UDP/RTP multicast | Todos os 6 casos da tabela passam                   |
| SPEC-NET-001a | URL com `?iface=` define interface de bind    | `iface: Some(Ipv4Addr)` no resultado                |
| SPEC-NET-002  | `UdpReceiver` — loop de recepção multicast    | Conecta em stream local; envia `Bytes` via canal    |
| SPEC-NET-002b | `SO_RCVBUF` aplicado antes do join            | Buffer de kernel configurável                       |
| SPEC-NET-002c | Timeout sem pacote → `NetEvent::Timeout`      | Não panic; não retorna Err                          |
| SPEC-NET-002d | Parada limpa com `IP_DROP_MEMBERSHIP`         | Leave do grupo antes de fechar socket               |
| SPEC-NET-003  | `RtpStripper` — remove header RTP (PT=33)     | Payload limpo enviado ao canal de saída             |
| SPEC-NET-003a | Detecção de pacotes fora de ordem             | `RtpEvent::OutOfOrder` emitido sem descartar pacote |

---

## Casos de Teste Obrigatórios

### SPEC-NET-001

| Entrada                                      | Resultado esperado                                               |
| -------------------------------------------- | ---------------------------------------------------------------- |
| `"udp://@239.1.1.1:1234"`                    | `Ok(UdpMulticast { group: 239.1.1.1, port: 1234, iface: None })` |
| `"rtp://@239.0.0.5:5004"`                    | `Ok(RtpMulticast { group: 239.0.0.5, port: 5004, iface: None })` |
| `"udp://10.0.0.1:1234"`                      | `Err(NetError::NotMulticast)`                                    |
| `"udp://@239.1.1.1:0"`                       | `Err(NetError::InvalidPort)`                                     |
| `"http://example.com"`                       | `Err(NetError::UnsupportedScheme)`                               |
| `"udp://@239.1.1.1:1234?iface=192.168.1.10"` | `Ok(UdpMulticast { .., iface: Some(192.168.1.10) })`             |

### SPEC-NET-003

| Cenário                                     | Comportamento                                  |
| ------------------------------------------- | ---------------------------------------------- |
| Buffer com RTP header válido (PT=33)        | Remove 12 bytes; passa payload                 |
| Buffer com CSRC count = 2                   | Remove 12 + 8 = 20 bytes                       |
| Buffer sem RTP (sync byte 0x47 no offset 0) | Passa integralmente                            |
| Sequence 0xFFFF → 0x0001 (wrap legítimo)    | Não emite OutOfOrder                           |
| Sequence 100 → 102 (pulo)                   | Emite `OutOfOrder { expected: 101, got: 102 }` |
