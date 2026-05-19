# Tasks: `crates/net`

> Fase: Alpha v0.1 (T01–T05) · Alpha v0.2 (T06–T07)  
> Gate: `cargo test -p net` verde + `cargo clippy -p net -- -D warnings`

---

## T01 — Scaffold do crate `net`

**O quê:** Criar `crates/net/Cargo.toml`, `src/lib.rs`, `src/error.rs` com os tipos `NetError`, `NetEvent`, `RtpEvent`.

**Onde:** `crates/net/`

**Done when:**
- `cargo check -p net` passa sem erros
- `NetError`, `NetEvent`, `RtpEvent` compilam com `#[derive(Debug)]`

**Testes:** nenhum nesta task (apenas estrutura)

---

## T02 — Implementar `StreamUrl::parse` (SPEC-NET-001)

**O quê:** `src/url.rs` com enum `StreamUrl` e método `parse`.

**Onde:** `crates/net/src/url.rs`

**Depende de:** T01

**Done when:**
- Todos os 6 casos de teste `spec_net_001_*` passam
- Validação de endereço multicast (`224.0.0.0/4`)
- Parsing de query string `?iface=`

**Testes:**
```
cargo test -p net spec_net_001
```

---

## T03 — Implementar `StopToken`

**O quê:** `src/stop.rs` com `StopToken` + `StopHandle` baseados em `Arc<AtomicBool>`.

**Onde:** `crates/net/src/stop.rs`

**Depende de:** T01

**Done when:**
- `StopHandle::stop()` muda estado
- `StopToken::is_stopped()` reflete mudança em outra thread

**Testes:** `spec_net_stop_token_basic`

---

## T04 — Implementar `UdpReceiver` (SPEC-NET-002)

**O quê:** `src/receiver.rs` com socket2, join/leave multicast, loop com timeout.

**Onde:** `crates/net/src/receiver.rs`

**Depende de:** T02, T03

**Done when:**
- `cargo test -p net spec_net_002` passa (teste de integração com loopback)
- `SO_RCVBUF` aplicado e logado se truncado (SPEC-NET-002b)
- Timeout emite `NetEvent::Timeout` sem retornar Err (SPEC-NET-002c)
- Leave multicast ao parar (SPEC-NET-002d)

**Testes:**
```
cargo test -p net spec_net_002
```

---

## T05 — Implementar `RtpStripper` (SPEC-NET-003)

**O quê:** `src/rtp.rs` com detecção de header RTP, remoção de header, detecção de out-of-order.

**Onde:** `crates/net/src/rtp.rs`

**Depende de:** T03

**Done when:**
- Todos os 5 casos `spec_net_003_*` passam
- Wrap-around de sequence number não dispara OutOfOrder

**Testes:**
```
cargo test -p net spec_net_003
```

---

## T06 — Teste de integração net+loopback

**O quê:** `tests/integration/net_loopback.rs` — envia pacotes UDP em loopback, verifica que `UdpReceiver` os entrega corretamente.

**Onde:** `crates/net/tests/`

**Depende de:** T04, T05

**Done when:**
- Teste envia 100 pacotes de 188 bytes via UDP loopback
- `UdpReceiver` recebe todos sem perda em ambiente de CI
- `RtpStripper` remove header corretamente em variante RTP do teste

**Testes:**
```
cargo test -p net --test integration
```

---

## T07 — Revisão de segurança e clippy

**O quê:** Auditar ausência de `unwrap()`/`expect()` em paths de dados externos; `cargo clippy -D warnings`.

**Depende de:** T01–T06

**Done when:**
- `cargo clippy -p net -- -D warnings` passa
- Nenhum `unwrap()`/`expect()` em caminhos de dados externos (apenas em testes e `Default::default()`)
- `cargo fmt --check` passa
