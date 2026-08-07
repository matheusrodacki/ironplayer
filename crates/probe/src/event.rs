//! Evento de monitoração: abertura, atualização e fechamento.
//!
//! SPEC-PROBE-008 · §6.2

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::severity::Severity;

/// Fase do evento no ciclo de vida.
///
/// SPEC-PROBE-008 — o mesmo `event_id` aparece em `open`, nos `update`
/// periódicos e no `close`; é isso que permite reconstruir a duração real a
/// partir de um `events.jsonl` truncado por queda de energia.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventPhase {
    Open,
    Update,
    Close,
}

/// Origem atribuída à ocorrência.
///
/// SPEC-PROBE-013 — perdas que coincidem com descarte local no mesmo segundo
/// são marcadas `local`, porque acusar a rede por um gargalo da própria probe
/// é o pior erro que um instrumento de diagnóstico pode cometer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventOrigin {
    /// Atribuído à rede sob investigação.
    #[default]
    Network,
    /// Atribuído à própria probe (canal cheio, socket, agendamento).
    Local,
}

/// Contexto que qualifica uma ocorrência e participa da chave de deduplicação.
///
/// SPEC-PROBE-008
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssrc: Option<String>,
    pub origin: EventOrigin,
}

impl EventContext {
    /// Contexto vazio de origem `network`.
    pub fn network() -> Self {
        Self::default()
    }

    /// Contexto de um PID específico.
    ///
    /// SPEC-PROBE-008
    pub fn pid(pid: u16) -> Self {
        Self {
            pid: Some(pid),
            ..Self::default()
        }
    }

    /// Marca a ocorrência como originada na própria probe.
    ///
    /// SPEC-PROBE-013
    pub fn with_origin(mut self, origin: EventOrigin) -> Self {
        self.origin = origin;
        self
    }

    /// Chave estável usada na deduplicação de eventos.
    ///
    /// **Não inclui `origin`**: a origem pode ser reclassificada de `network`
    /// para `local` no meio de uma rajada, e se ela entrasse na chave isso
    /// abriria um segundo evento para o mesmo problema — exatamente o que
    /// SPEC-PROBE-008 proíbe.
    pub fn dedupe_key(&self) -> String {
        let mut key = String::with_capacity(24);
        match self.pid {
            Some(p) => {
                let _ = write!(key, "pid={p}");
            }
            None => key.push_str("pid=*"),
        }
        match self.service_id {
            Some(s) => {
                let _ = write!(key, ";svc={s}");
            }
            None => key.push_str(";svc=*"),
        }
        match &self.ssrc {
            Some(s) => {
                let _ = write!(key, ";ssrc={s}");
            }
            None => key.push_str(";ssrc=*"),
        }
        key
    }

    /// Descrição curta para o event log da UI (§8.2).
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(p) = self.pid {
            parts.push(format!("pid {p}"));
        }
        if let Some(s) = self.service_id {
            parts.push(format!("svc {s}"));
        }
        if let Some(s) = &self.ssrc {
            parts.push(format!("ssrc {s}"));
        }
        if self.origin == EventOrigin::Local {
            parts.push("origin=local".to_string());
        }
        parts.join(" · ")
    }
}

/// Uma linha de `events.jsonl`.
///
/// SPEC-PROBE-008 · §6.2
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeEvent {
    pub event_id: String,
    pub check_id: String,
    pub phase: EventPhase,
    pub severity: Severity,
    pub ts_utc: DateTime<Utc>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Ocorrências agregadas desde a abertura (SPEC-PROBE-008).
    pub count: u64,
    /// Duração desde a abertura, em ms (0 na fase `open`).
    pub duration_ms: u64,
    /// Valor medido que disparou/manteve o evento.
    pub measured: f64,
    /// Limiar vigente no perfil.
    pub threshold: f64,
    pub unit: String,
    pub context: EventContext,
    /// Versão do perfil que produziu o evento (SPEC-PROBE-007).
    pub profile_version: u32,
}

impl ProbeEvent {
    /// Serializa como uma única linha JSON (formato `events.jsonl`).
    ///
    /// Falha de serialização é reportada como `None` em vez de panic — um
    /// evento perdido é preferível a derrubar a sessão (RNF-PRB-003).
    ///
    /// SPEC-PROBE-006
    pub fn to_jsonl(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

/// Gerador de `event_id` monotonicamente crescente e ordenável por texto.
///
/// Formato: `<ms UTC em base32 Crockford, 10 dígitos><contador, 6 dígitos>`.
/// Ordenar as linhas de `events.jsonl` por `event_id` reproduz a ordem
/// cronológica — a mesma propriedade de um ULID, sem acrescentar dependência.
///
/// SPEC-PROBE-008
#[derive(Debug, Default)]
pub struct EventIdGen {
    counter: AtomicU64,
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

impl EventIdGen {
    /// Cria um gerador zerado.
    pub fn new() -> Self {
        Self::default()
    }

    /// Emite o próximo id para um instante de parede.
    ///
    /// SPEC-PROBE-008
    pub fn next(&self, ts: DateTime<Utc>) -> String {
        let ms = ts.timestamp_millis().max(0) as u64;
        let seq = self.counter.fetch_add(1, Ordering::Relaxed) % 1_000_000;

        let mut out = [b'0'; 16];
        let mut v = ms;
        for slot in out[..10].iter_mut().rev() {
            *slot = CROCKFORD[(v & 0x1F) as usize];
            v >>= 5;
        }
        let mut s = seq;
        for slot in out[10..].iter_mut().rev() {
            *slot = b'0' + (s % 10) as u8;
            s /= 10;
        }
        // Todos os bytes vêm de tabelas ASCII acima.
        String::from_utf8_lossy(&out).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("timestamp")
    }

    /// SPEC-PROBE-008 — a chave de dedupe ignora a origem, para que
    /// reclassificar network→local não abra um segundo evento.
    #[test]
    fn spec_probe_008_dedupe_key_ignores_origin() {
        let a = EventContext::pid(6100);
        let b = EventContext::pid(6100).with_origin(EventOrigin::Local);
        assert_eq!(a.dedupe_key(), b.dedupe_key());
    }

    /// SPEC-PROBE-008 — contextos de PIDs diferentes são eventos diferentes.
    #[test]
    fn spec_probe_008_dedupe_key_separates_pids() {
        assert_ne!(
            EventContext::pid(6100).dedupe_key(),
            EventContext::pid(6102).dedupe_key()
        );
    }

    /// SPEC-PROBE-008 — ids são únicos e ordenáveis lexicograficamente.
    #[test]
    fn spec_probe_008_event_ids_sort_chronologically() {
        let gen = EventIdGen::new();
        let early = gen.next(utc(1_700_000_000));
        let late = gen.next(utc(1_700_000_001));
        assert_eq!(early.len(), 16);
        assert!(early < late, "{early} deveria ordenar antes de {late}");

        // Mesmo instante: o contador desempata mantendo unicidade.
        let a = gen.next(utc(1_700_000_002));
        let b = gen.next(utc(1_700_000_002));
        assert_ne!(a, b);
        assert!(a < b);
    }

    /// §6.2 — a linha JSONL carrega os campos exigidos pela spec.
    #[test]
    fn spec_probe_006_jsonl_line_has_required_fields() {
        let ev = ProbeEvent {
            event_id: "01J0000000000000".into(),
            check_id: "rtp_missing".into(),
            phase: EventPhase::Open,
            severity: Severity::Error,
            ts_utc: utc(1_700_000_000),
            first_seen: utc(1_700_000_000),
            last_seen: utc(1_700_000_000),
            count: 130,
            duration_ms: 0,
            measured: 130.0,
            threshold: 0.0,
            unit: "pkts".into(),
            context: EventContext {
                ssrc: Some("0x1A2B3C4D".into()),
                ..EventContext::network()
            },
            profile_version: 1,
        };

        let line = ev.to_jsonl().expect("serializa");
        assert!(!line.contains('\n'), "JSONL é uma linha só");
        for needle in [
            "\"check_id\":\"rtp_missing\"",
            "\"phase\":\"open\"",
            "\"severity\":\"error\"",
            "\"count\":130",
            "\"profile_version\":1",
            "\"origin\":\"network\"",
        ] {
            assert!(line.contains(needle), "faltou {needle} em {line}");
        }

        let back: ProbeEvent = serde_json::from_str(&line).expect("desserializa");
        assert_eq!(back, ev);
    }
}
