# TDD: `crates/net` — Recepção UDP/RTP Multicast

- **Data:** 2026-05-19
- **Status:** Accepted
- **Deciders:** IronPlayer Core Team
- **Spec-IDs:** SPEC-NET-001, SPEC-NET-002, SPEC-NET-003
- **Fase:** Alpha v0.1 (NET-001/002) · Alpha v0.2 (NET-003)

---

## Contexto e Problema

O IronPlayer precisa receber streams MPEG-TS via UDP multicast (protocolo mais comum em redes de broadcast profissional) e RTP/UDP (encapsulamento comum em cabeças de rede IPTV). O recebimento precisa ser confiável, com controle de backpressure e parada limpa de socket.

A camada `net` é **isolada de todo o resto da aplicação** — não depende de `ts`, `av` ou `ui`. Isso garante testabilidade unitária sem fixtures de vídeo.

---

## Escopo

**In-scope:**
- Parse de URLs `udp://` e `rtp://` com endereços multicast IPv4
- Socket UDP com join de grupo multicast (`IP_ADD_MEMBERSHIP`)
- Configuração de buffer de kernel (`SO_RCVBUF`)
- Loop de recepção com timeout configurável
- Remoção de header RTP (version=2, PT=33) dos payloads
- Detecção de pacotes RTP fora de ordem (sequence number)
- Parada limpa via `StopToken` com `IP_DROP_MEMBERSHIP`

**Out-of-scope:**
- Multicast IPv6
- IGMP snooping / configuração de roteamento
- Retransmissão / ARQ (sem FEC)
- Input via TCP / HTTP / arquivo

---

## Solução Técnica

### Estrutura do crate

```
crates/net/
├── Cargo.toml
└── src/
    ├── lib.rs          # re-exports públicos
    ├── url.rs          # SPEC-NET-001: StreamUrl + parse
    ├── receiver.rs     # SPEC-NET-002: UdpReceiver
    ├── rtp.rs          # SPEC-NET-003: RtpStripper
    └── error.rs        # NetError, NetEvent, RtpEvent
```

### Dependências (`Cargo.toml`)

```toml
[dependencies]
socket2    = "0.5"          # SO_RCVBUF, IP_ADD/DROP_MEMBERSHIP
tokio      = { workspace = true }
crossbeam-channel = { workspace = true }
bytes      = { workspace = true }
thiserror  = { workspace = true }
tracing    = { workspace = true }

[dev-dependencies]
tokio      = { workspace = true, features = ["test-util"] }
```

> **Decisão:** usar `socket2` em vez da API padrão da stdlib para acessar `SO_RCVBUF` e opções de multicast de forma portável.  
> **Decisão:** o loop de recepção roda em thread blocking (`tokio::task::spawn_blocking`) — recepção UDP é I/O síncrona de alta frequência; `async` adicionaria overhead desnecessário.

---

## Contratos de Interface

### `StreamUrl` (SPEC-NET-001)

```rust
pub enum StreamUrl {
    UdpMulticast { group: Ipv4Addr, port: u16, iface: Option<Ipv4Addr> },
    RtpMulticast { group: Ipv4Addr, port: u16, iface: Option<Ipv4Addr> },
}

impl StreamUrl {
    pub fn parse(s: &str) -> Result<Self, NetError>;
}
```

**Invariante:** `group` é sempre um endereço multicast (224.0.0.0/4). Qualquer outro endereço retorna `NetError::NotMulticast`. Porta 0 retorna `NetError::InvalidPort`.

### `UdpReceiver` (SPEC-NET-002)

```rust
pub struct ReceiverConfig {
    pub buf_size:   usize,   // default: 4_194_304 (4 MB)
    pub timeout_ms: u64,     // default: 5_000
}

pub struct UdpReceiver {
    url:    StreamUrl,
    tx:     crossbeam_channel::Sender<Bytes>,
    events: crossbeam_channel::Sender<NetEvent>,
    cfg:    ReceiverConfig,
}

impl UdpReceiver {
    pub fn new(url: StreamUrl, tx: Sender<Bytes>, events: Sender<NetEvent>, cfg: ReceiverConfig) -> Self;
    pub fn run(self, stop: StopToken) -> Result<(), NetError>;
}
```

**Ciclo de vida do socket:**
1. `socket2::Socket::new(AF_INET, SOCK_DGRAM, None)`
2. `set_recv_buffer_size(cfg.buf_size)` (SPEC-NET-002b)
3. `bind(0.0.0.0:port)`
4. `join_multicast_v4(group, iface_or_any)` → `Err(NetError::JoinFailed)` se falhar
5. Loop com `set_read_timeout(Some(timeout_ms))`:
   - Timeout → emite `NetEvent::Timeout`, continua (SPEC-NET-002c)
   - `EAGAIN` / `EINTR` → loga, continua
   - Erro fatal → retorna `Err`
   - `StopToken` sinalizado → break
6. `leave_multicast_v4` + fecha socket (SPEC-NET-002d)

### `StopToken`

```rust
/// Token de cancelamento baseado em `Arc<AtomicBool>`.
/// Permite parar o loop de recepção da thread da UI.
#[derive(Clone)]
pub struct StopToken(Arc<AtomicBool>);

impl StopToken {
    pub fn new() -> (Self, StopHandle);
    pub fn is_stopped(&self) -> bool;
}

pub struct StopHandle(Arc<AtomicBool>);
impl StopHandle {
    pub fn stop(&self);
}
```

### `RtpStripper` (SPEC-NET-003)

```rust
pub struct RtpStripper {
    rx:     crossbeam_channel::Receiver<Bytes>,
    tx:     crossbeam_channel::Sender<Bytes>,
    events: crossbeam_channel::Sender<RtpEvent>,
}

impl RtpStripper {
    pub fn new(rx: Receiver<Bytes>, tx: Sender<Bytes>, events: Sender<RtpEvent>) -> Self;
    pub fn run(self, stop: StopToken);
}
```

**Algoritmo de detecção RTP (SPEC-NET-003):**
```
Se buf.len() >= 12 E buf[0] >> 6 == 2 (version=2) E (buf[1] & 0x7F) == 33 (PT=33):
    cc = buf[0] & 0x0F   (CSRC count)
    header_len = 12 + 4 * cc
    payload = buf[header_len..]
    verificar sequence number para detecção de out-of-order
Senão:
    payload = buf (modo UDP puro; sync byte 0x47 no offset 0)
```

**Detecção de out-of-order:**
```
next_expected = (last_seq + 1) & 0xFFFF
se seq == next_expected → OK, atualiza last_seq
se seq == last_seq → emite RtpEvent::Duplicate
senão → emite RtpEvent::OutOfOrder { expected: next_expected, got: seq }
Wrap-around: (last_seq == 0xFFFF && seq == 0x0000) é legítimo, não é OutOfOrder
```

---

## Tipos de Erro e Eventos

```rust
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    NotMulticast(Ipv4Addr),
    InvalidPort,
    UnsupportedScheme(String),
    SocketError(#[from] std::io::Error),
    JoinFailed(std::io::Error),
}

pub enum NetEvent {
    Connected { group: Ipv4Addr, port: u16 },
    Timeout,
    Disconnected,
    UdpBufferOverflow { dropped_bytes: u64 },
}

pub enum RtpEvent {
    OutOfOrder { expected: u16, got: u16 },
    Duplicate  { sequence: u16 },
}
```

---

## Estratégia de Testes

### Testes unitários (`crates/net/src/url.rs`)

Todos os 6 casos do `StreamUrl::parse` como tabela com `rstest::rstest`.

```rust
// spec_net_001_valid_udp_multicast
// spec_net_001_valid_rtp_multicast
// spec_net_001_not_multicast
// spec_net_001_invalid_port_zero
// spec_net_001_unsupported_scheme
// spec_net_001_with_iface
```

### Testes unitários (`crates/net/src/rtp.rs`)

Todos os 5 cenários do `RtpStripper` com buffers sintéticos.

```rust
// spec_net_003_rtp_valid_header_stripped
// spec_net_003_rtp_csrc_count_2
// spec_net_003_udp_passthrough
// spec_net_003_sequence_wrap_not_out_of_order
// spec_net_003_sequence_jump_emits_event
```

### Teste de integração (`tests/net_receiver.rs`)

Cria socket UDP local no loopback, envia pacotes, verifica recepção via `UdpReceiver`. Não depende de multicast real.

---

## Considerações de Segurança

- **Nenhum dado de rede é interpretado como código** — apenas bytes copiados para `Bytes`.
- Buffer de recepção alocado com tamanho fixo (não dinâmico baseado em dados externos).
- `UdpReceiver::run` nunca faz `unwrap()`/`expect()` em dados externos; todos os erros são tratados via `Result`.
- `SO_RCVBUF` limita o tamanho do buffer de kernel, prevenindo consumo excessivo de memória.

---

## Riscos e Mitigações

| Risco                                                                                   | Mitigação                                                         |
| --------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| `SO_RCVBUF` ignorado pelo kernel (sistemas com limite em `/proc/sys/net/core/rmem_max`) | Logar aviso se `getsockopt` retornar valor menor que o solicitado |
| Driver de rede não suportar multicast na interface selecionada                          | `JoinFailed` propagado ao usuário via `NetEvent::Error`           |
| Pacotes RTP com extensão de header (X bit) não suportados em v0.1                       | Tratar X=1 como modo UDP puro por segurança                       |
