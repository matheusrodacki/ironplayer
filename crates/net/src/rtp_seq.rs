//! Máquina de sequência RTP por SSRC.
//!
//! É a peça central da camada 1: perda e reordenação dependem de sequence
//! number, não de relógio, então são as métricas **exatas** desta camada — as
//! únicas comparáveis com uma probe de captura em hardware (§4).
//!
//! Modelo derivado de RFC 3550 §A.1, estendido com uma janela de reconciliação
//! para não contar reordenação como perda: uma lacuna vira **perda provisória**
//! e só é confirmada quando a janela vence sem o pacote aparecer.
//!
//! SPEC-PROBE-IP-019 … SPEC-PROBE-IP-024

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Salto de sequência **para a frente** acima do qual a fonte é considerada
/// reiniciada, e não 60 000 pacotes perdidos.
///
/// SPEC-PROBE-IP-024 — RFC 3550 §A.1.
pub const MAX_DROPOUT: u16 = 3000;

/// Distância **para trás** dentro da qual um pacote ainda é candidato a
/// reordenação; além disso é `too_old`.
///
/// SPEC-PROBE-IP-023 — RFC 3550 §A.1.
pub const MAX_MISORDER: u16 = 100;

/// Quantos sequence numbers recentes a janela de duplicatas cobre.
///
/// 1024 pacotes são ~0,7 s num CBR de 15 Mbps com 7 TS/datagrama — folga
/// confortável sobre a janela de reordenação default (200 ms), sem alocar.
const SEEN_CAPACITY: u64 = 1024;

/// O que a chegada de um pacote significou para o fluxo.
///
/// SPEC-PROBE-IP-019 … SPEC-PROBE-IP-024
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOutcome {
    /// Primeiro pacote deste SSRC.
    Init,
    /// Sequência contínua.
    InOrder,
    /// Lacuna: `missing` sequence numbers entraram em **perda provisória**.
    Gap { missing: u32 },
    /// Uma lacuna pendente foi reconciliada dentro da janela.
    Reordered,
    /// Sequence number já recebido.
    Duplicate,
    /// Abaixo da janela de reordenação — não é perda negativa.
    TooOld,
    /// Salto grande: a fonte reiniciou, os contadores rebaseiam.
    SourceRestart { from: u16, to: u16 },
}

/// Contadores publicados a cada tick.
///
/// SPEC-PROBE-IP-019 … SPEC-PROBE-IP-024 · §6
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpSeqCounters {
    pub ssrc: u32,
    pub received: u64,
    pub dup: u64,
    pub out_of_order: u64,
    pub too_old: u64,
    /// Lacunas que venceram a janela de reconciliação.
    pub missing_confirmed: u64,
    pub source_restarts: u64,
    /// Lacunas ainda dentro da janela (nem perda nem reordenação, ainda).
    pub pending: u64,
}

impl RtpSeqCounters {
    /// Razão de perda confirmada sobre o esperado.
    ///
    /// Denominador é `recebidos + perdidos`: usar só os recebidos daria uma
    /// razão que cresce sem limite quando tudo se perde, e o limiar de
    /// `rtp_loss_ratio` (1e-4) deixaria de significar o que a spec diz (§8).
    pub fn loss_ratio(&self) -> Option<f64> {
        let expected = self.received + self.missing_confirmed;
        (expected > 0).then(|| self.missing_confirmed as f64 / expected as f64)
    }
}

/// Estado de sequência de **um** SSRC.
///
/// SPEC-PROBE-IP-019 — o estado é por SSRC, nunca global: sem isso, uma troca
/// de fonte produziria uma explosão de perda que nunca existiu.
#[derive(Debug)]
pub struct RtpSeqState {
    ssrc: u32,
    base_ext: u64,
    max_seq: u16,
    /// Voltas completas de 16 bits.
    cycles: u64,
    received: u64,
    dup: u64,
    out_of_order: u64,
    too_old: u64,
    missing_confirmed: u64,
    source_restarts: u64,
    /// Sequence estendido pendente → instante em que a lacuna foi observada.
    pending_gaps: BTreeMap<u64, Instant>,
    seen: SeenWindow,
    /// Sequência esperada de um possível reinício **para trás** (RFC 3550
    /// chama de `bad_seq`).
    bad_seq: Option<u16>,
    bad_count: u16,
    primed: bool,
}

impl RtpSeqState {
    /// Cria o estado de um SSRC, ainda sem pacote algum.
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            base_ext: 0,
            max_seq: 0,
            cycles: 0,
            received: 0,
            dup: 0,
            out_of_order: 0,
            too_old: 0,
            missing_confirmed: 0,
            source_restarts: 0,
            pending_gaps: BTreeMap::new(),
            seen: SeenWindow::default(),
            bad_seq: None,
            bad_count: 0,
            primed: false,
        }
    }

    /// SSRC deste fluxo.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Contadores para o tick.
    pub fn counters(&self) -> RtpSeqCounters {
        RtpSeqCounters {
            ssrc: self.ssrc,
            received: self.received,
            dup: self.dup,
            out_of_order: self.out_of_order,
            too_old: self.too_old,
            missing_confirmed: self.missing_confirmed,
            source_restarts: self.source_restarts,
            pending: self.pending_gaps.len() as u64,
        }
    }

    /// Contabiliza a chegada de um sequence number.
    ///
    /// SPEC-PROBE-IP-020 … SPEC-PROBE-IP-024
    pub fn observe(&mut self, seq: u16, now: Instant) -> SeqOutcome {
        if !self.primed {
            self.rebase(seq);
            self.received = 1;
            return SeqOutcome::Init;
        }

        let position = match position(self.max_seq, seq) {
            Position::Same => {
                self.dup += 1;
                return SeqOutcome::Duplicate;
            }
            Position::FarAway => {
                // SPEC-PROBE-IP-024 — 100 → 40 000 é reinício de fonte, não
                // 39 899 perdas.  As lacunas pendentes são descartadas sem
                // contabilizar: depois de um reinício a base de sequência não
                // significa mais nada, e atribuir aquilo à rede seria inventar
                // perda exatamente no instante em que o estado é menos
                // confiável.
                let from = self.max_seq;
                self.rebase(seq);
                self.received += 1;
                self.source_restarts += 1;
                return SeqOutcome::SourceRestart { from, to: seq };
            }
            other => other,
        };

        if let Position::Forward(delta) = position {
            // Avanço normal (inclui o wrap 0xFFFF → 0x0000).
            if seq < self.max_seq {
                self.cycles += 1;
            }
            let ext = self.cycles * 65_536 + u64::from(seq);
            let missing = delta - 1;
            for step in 1..delta {
                self.pending_gaps.insert(ext - u64::from(step), now);
            }
            self.max_seq = seq;
            self.seen.advance_to(ext);
            self.seen.mark(ext);
            self.received += 1;
            self.bad_seq = None;
            self.bad_count = 0;

            return if missing == 0 {
                SeqOutcome::InOrder
            } else {
                SeqOutcome::Gap { missing }
            };
        }

        // Para trás — o pacote é anterior ao máximo observado.
        if let Some(ext) = self.backward_ext(seq) {
            // SPEC-PROBE-IP-020 — o critério de reordenação é **tempo**, não
            // distância: se a lacuna ainda está pendente é porque a janela de
            // reconciliação não venceu, e `expire` é quem a fecha.
            if self.pending_gaps.remove(&ext).is_some() {
                self.seen.mark(ext);
                self.received += 1;
                self.out_of_order += 1;
                self.bad_seq = None;
                self.bad_count = 0;
                return SeqOutcome::Reordered;
            }
            if self.seen.contains(ext) {
                self.dup += 1;
                return SeqOutcome::Duplicate;
            }
        }

        // SPEC-PROBE-IP-023 — abaixo da janela: `too_old`, nunca perda
        // negativa.  A contagem de `bad_seq` existe para o caso em que a fonte
        // reinicia **para trás**: sem ela, todo pacote da nova sequência viraria
        // `too_old` para sempre (RFC 3550 §A.1).
        self.too_old += 1;
        match self.bad_seq {
            Some(expected) if expected == seq => {
                self.bad_count += 1;
                if self.bad_count >= MAX_MISORDER {
                    let from = self.max_seq;
                    self.rebase(seq);
                    self.received += 1;
                    self.source_restarts += 1;
                    return SeqOutcome::SourceRestart { from, to: seq };
                }
            }
            _ => self.bad_count = 1,
        }
        self.bad_seq = Some(seq.wrapping_add(1));
        SeqOutcome::TooOld
    }

    /// Confirma como perda as lacunas mais velhas que `window`.
    ///
    /// Devolve quantas foram confirmadas nesta chamada — é o delta que vai para
    /// o check `rtp_missing` e para a coluna `rtp_missing_delta` do CSV.
    ///
    /// SPEC-PROBE-IP-021
    pub fn expire(&mut self, now: Instant, window: Duration) -> u64 {
        let mut confirmed = 0u64;
        self.pending_gaps.retain(|_, since| {
            if now.saturating_duration_since(*since) >= window {
                confirmed += 1;
                false
            } else {
                true
            }
        });
        self.missing_confirmed += confirmed;
        confirmed
    }

    /// Confirma **todas** as lacunas pendentes (fim de sessão, troca de SSRC).
    pub fn flush(&mut self) -> u64 {
        let confirmed = self.pending_gaps.len() as u64;
        self.pending_gaps.clear();
        self.missing_confirmed += confirmed;
        confirmed
    }

    /// Reinicia a base de sequência mantendo os contadores históricos.
    fn rebase(&mut self, seq: u16) {
        self.cycles = 0;
        self.max_seq = seq;
        self.base_ext = u64::from(seq);
        self.pending_gaps.clear();
        self.seen = SeenWindow::default();
        self.seen.advance_to(u64::from(seq));
        self.seen.mark(u64::from(seq));
        self.bad_seq = None;
        self.bad_count = 0;
        self.primed = true;
    }

    /// Sequence estendido de um pacote anterior ao máximo.
    fn backward_ext(&self, seq: u16) -> Option<u64> {
        let base = if seq > self.max_seq {
            // O máximo acabou de dar a volta; este pacote é da volta anterior.
            self.cycles.checked_sub(1)?
        } else {
            self.cycles
        };
        Some(base * 65_536 + u64::from(seq))
    }
}

/// Onde um sequence number cai em relação ao máximo já observado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    /// Igual ao máximo.
    Same,
    /// `n` posições à frente (inclui o wrap de 16 bits).
    Forward(u32),
    /// `n` posições atrás.
    Backward(u32),
    /// Longe demais nos dois sentidos para ser a mesma sequência.
    FarAway,
}

/// Classifica `to` em relação a `from` no espaço circular de 16 bits.
///
/// A janela válida é [`MAX_DROPOUT`] **nos dois sentidos**, e é o que reconcilia
/// os dois casos que a spec fixa: 100 → 40 000 fica fora da janela nos dois
/// sentidos e é reinício de fonte (SPEC-PROBE-IP-024), enquanto seq 50 com
/// `max_seq = 300` está 250 atrás, dentro da janela, e é `too_old`
/// (SPEC-PROBE-IP-023).  Escolher pelo caminho circular mais curto — o reflexo
/// natural — classificaria o primeiro como "25 636 atrás" e perderia o reinício.
fn position(from: u16, to: u16) -> Position {
    let forward = u32::from(to.wrapping_sub(from));
    if forward == 0 {
        return Position::Same;
    }
    let limit = u32::from(MAX_DROPOUT);
    if forward <= limit {
        return Position::Forward(forward);
    }
    let backward = 65_536 - forward;
    if backward <= limit {
        return Position::Backward(backward);
    }
    Position::FarAway
}

/// Janela circular de sequence numbers já recebidos.
///
/// Existe só para distinguir **duplicata** de **`too_old`**: sem ela, o segundo
/// envio de um pacote antigo seria contado como pacote atrasado, e a diferença
/// entre "a rede duplicou" e "a rede atrasou" some justamente no diagnóstico.
#[derive(Debug, Default)]
struct SeenWindow {
    bits: [u64; (SEEN_CAPACITY / 64) as usize],
    max: u64,
    primed: bool,
}

impl SeenWindow {
    fn advance_to(&mut self, ext: u64) {
        if !self.primed {
            self.bits = [0; (SEEN_CAPACITY / 64) as usize];
            self.max = ext;
            self.primed = true;
            return;
        }
        if ext <= self.max {
            return;
        }
        // Só os slots que acabaram de rotacionar para dentro precisam ser
        // limpos; um salto maior que a capacidade zera tudo.
        let steps = (ext - self.max).min(SEEN_CAPACITY);
        for i in 1..=steps {
            self.clear(self.max + i);
        }
        self.max = ext;
    }

    fn in_window(&self, ext: u64) -> bool {
        self.primed && ext <= self.max && self.max - ext < SEEN_CAPACITY
    }

    fn slot(ext: u64) -> (usize, u64) {
        let idx = (ext % SEEN_CAPACITY) as usize;
        (idx / 64, 1u64 << (idx % 64))
    }

    fn mark(&mut self, ext: u64) {
        if !self.in_window(ext) {
            return;
        }
        let (word, bit) = Self::slot(ext);
        self.bits[word] |= bit;
    }

    fn clear(&mut self, ext: u64) {
        let (word, bit) = Self::slot(ext);
        self.bits[word] &= !bit;
    }

    fn contains(&self, ext: u64) -> bool {
        if !self.in_window(ext) {
            return false;
        }
        let (word, bit) = Self::slot(ext);
        self.bits[word] & bit != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_millis(200);

    fn state() -> (RtpSeqState, Instant) {
        (RtpSeqState::new(0x1A2B_3C4D), Instant::now())
    }

    /// §7 — sequência contínua de 1000 pacotes não produz nenhuma anomalia.
    #[test]
    fn spec_probe_ip_019_continuous_sequence_has_no_anomalies() {
        let (mut s, t0) = state();
        for i in 0..1000u16 {
            s.observe(1000u16.wrapping_add(i), t0);
        }
        s.expire(t0 + Duration::from_secs(1), WINDOW);

        let c = s.counters();
        assert_eq!(c.received, 1000);
        assert_eq!(c.missing_confirmed, 0);
        assert_eq!(c.dup, 0);
        assert_eq!(c.out_of_order, 0);
        assert_eq!(c.too_old, 0);
        assert_eq!(c.loss_ratio(), Some(0.0));
    }

    /// SPEC-PROBE-IP-021 — 100 → 102 sem o 101: a lacuna vira perda **confirmada**
    /// só depois da janela de reconciliação.
    #[test]
    fn spec_probe_ip_021_unreconciled_gap_becomes_confirmed_loss() {
        let (mut s, t0) = state();
        s.observe(100, t0);
        assert_eq!(s.observe(102, t0), SeqOutcome::Gap { missing: 1 });

        // Ainda dentro da janela: nada confirmado.
        assert_eq!(s.expire(t0 + Duration::from_millis(100), WINDOW), 0);
        assert_eq!(s.counters().missing_confirmed, 0);
        assert_eq!(s.counters().pending, 1);

        assert_eq!(s.expire(t0 + Duration::from_millis(250), WINDOW), 1);
        assert_eq!(s.counters().missing_confirmed, 1);
        assert_eq!(s.counters().pending, 0);
    }

    /// SPEC-PROBE-IP-020 — 100 → 102 → 101 em 50 ms é reordenação, não perda.
    #[test]
    fn spec_probe_ip_020_reorder_within_window() {
        let (mut s, t0) = state();
        s.observe(100, t0);
        s.observe(102, t0);
        assert_eq!(
            s.observe(101, t0 + Duration::from_millis(50)),
            SeqOutcome::Reordered
        );
        s.expire(t0 + Duration::from_secs(1), WINDOW);

        let c = s.counters();
        assert_eq!(c.out_of_order, 1);
        assert_eq!(c.missing_confirmed, 0);
        assert_eq!(c.received, 3);
    }

    /// SPEC-PROBE-IP-023 — 100 → 102 → 101 depois de 500 ms: a lacuna já virou
    /// perda, e o retardatário é `too_old`.
    #[test]
    fn spec_probe_ip_023_late_arrival_is_too_old_not_negative_loss() {
        let (mut s, t0) = state();
        s.observe(100, t0);
        s.observe(102, t0);
        assert_eq!(s.expire(t0 + Duration::from_millis(300), WINDOW), 1);
        assert_eq!(
            s.observe(101, t0 + Duration::from_millis(500)),
            SeqOutcome::TooOld
        );

        let c = s.counters();
        assert_eq!(c.missing_confirmed, 1);
        assert_eq!(c.too_old, 1);
        assert_eq!(c.out_of_order, 0);
    }

    /// SPEC-PROBE-IP-023 — seq 50 com `max_seq = 300` é `too_old`, e não uma
    /// perda de sinal negativo.
    #[test]
    fn spec_probe_ip_023_far_behind_packet_is_too_old() {
        let (mut s, t0) = state();
        for seq in 100..=300u16 {
            s.observe(seq, t0);
        }
        assert_eq!(s.observe(50, t0), SeqOutcome::TooOld);
        assert_eq!(s.counters().too_old, 1);
        assert_eq!(s.counters().missing_confirmed, 0);
    }

    /// SPEC-PROBE-IP-022 — o mesmo seq duas vezes é duplicata.
    #[test]
    fn spec_probe_ip_022_duplicate_is_counted_once() {
        let (mut s, t0) = state();
        s.observe(100, t0);
        assert_eq!(s.observe(100, t0), SeqOutcome::Duplicate);
        assert_eq!(s.counters().dup, 1);
        assert_eq!(s.counters().received, 1, "duplicata não conta como recebido");

        // Duplicata de um pacote antigo (não o máximo) também é duplicata, e
        // não `too_old`: a janela de recebidos é o que separa as duas coisas.
        for seq in 101..=140u16 {
            s.observe(seq, t0);
        }
        assert_eq!(s.observe(120, t0), SeqOutcome::Duplicate);
        assert_eq!(s.counters().dup, 2);
        assert_eq!(s.counters().too_old, 0);
    }

    /// §7 — wrap 0xFFFF → 0x0000 incrementa o ciclo sem contabilizar perda.
    #[test]
    fn spec_probe_ip_019_sequence_wrap_costs_nothing() {
        let (mut s, t0) = state();
        s.observe(0xFFFE, t0);
        assert_eq!(s.observe(0xFFFF, t0), SeqOutcome::InOrder);
        assert_eq!(s.observe(0x0000, t0), SeqOutcome::InOrder);
        assert_eq!(s.observe(0x0001, t0), SeqOutcome::InOrder);
        s.expire(t0 + Duration::from_secs(1), WINDOW);

        let c = s.counters();
        assert_eq!(c.missing_confirmed, 0);
        assert_eq!(c.received, 4);
        assert_eq!(s.cycles, 1, "uma volta completa");
    }

    /// SPEC-PROBE-IP-020 — reordenação **através** do wrap continua sendo
    /// reordenação: 0xFFFF → 0x0001 → 0x0000.
    #[test]
    fn spec_probe_ip_020_reorder_across_the_wrap() {
        let (mut s, t0) = state();
        s.observe(0xFFFF, t0);
        assert_eq!(s.observe(0x0001, t0), SeqOutcome::Gap { missing: 1 });
        assert_eq!(
            s.observe(0x0000, t0 + Duration::from_millis(10)),
            SeqOutcome::Reordered
        );
        assert_eq!(s.counters().out_of_order, 1);
        assert_eq!(s.expire(t0 + Duration::from_secs(1), WINDOW), 0);
    }

    /// SPEC-PROBE-IP-024 — salto de 100 para 40 000 é reinício de fonte, não
    /// 39 899 perdas.
    #[test]
    fn spec_probe_ip_024_forward_jump_is_a_source_restart() {
        let (mut s, t0) = state();
        s.observe(100, t0);
        assert_eq!(
            s.observe(40_000, t0),
            SeqOutcome::SourceRestart {
                from: 100,
                to: 40_000
            }
        );
        s.expire(t0 + Duration::from_secs(1), WINDOW);

        let c = s.counters();
        assert_eq!(c.missing_confirmed, 0, "reinício não é perda");
        assert_eq!(c.source_restarts, 1);
        assert_eq!(c.received, 2);

        // Depois do rebase a contagem volta a funcionar normalmente.
        assert_eq!(s.observe(40_001, t0), SeqOutcome::InOrder);
    }

    /// SPEC-PROBE-IP-024 — reinício **para trás** também rebaseia, em vez de
    /// produzir `too_old` para sempre.
    ///
    /// Dois caminhos, porque a fonte pode voltar para perto ou para longe:
    /// um salto que sai da janela nos dois sentidos é reinício imediato; um
    /// salto que fica dentro dela só se revela pela corrida de `bad_seq`.
    #[test]
    fn spec_probe_ip_024_backward_restart_rebases() {
        // Longe: 5 100 → 0 está fora da janela nos dois sentidos.
        let (mut far, t0) = state();
        for seq in 5_000..5_100u16 {
            far.observe(seq, t0);
        }
        assert!(matches!(
            far.observe(0, t0),
            SeqOutcome::SourceRestart { from: 5_099, to: 0 }
        ));
        assert_eq!(far.observe(1, t0), SeqOutcome::InOrder);

        // Perto: 2 000 → 500 continua dentro da janela de 3 000, então os
        // primeiros pacotes são `too_old` até a sequência contínua denunciar o
        // reinício (RFC 3550 §A.1, `bad_seq`).
        let (mut near, t0) = state();
        for seq in 1_900..2_000u16 {
            near.observe(seq, t0);
        }
        let mut restart = None;
        for seq in 500..500 + MAX_MISORDER + 5 {
            if let SeqOutcome::SourceRestart { .. } = near.observe(seq, t0) {
                restart = Some(seq);
                break;
            }
        }
        let restart = restart.expect("reinício para trás precisa rebasear");
        assert_eq!(near.counters().source_restarts, 1);
        assert!(near.counters().too_old >= u64::from(MAX_MISORDER) - 1);
        assert_eq!(near.observe(restart + 1, t0), SeqOutcome::InOrder);
    }

    /// SPEC-PROBE-IP-023 · SPEC-PROBE-IP-024 — a classificação circular é o que
    /// separa "pacote velho" de "fonte reiniciada"; escolher o caminho mais
    /// curto no círculo confundiria os dois.
    #[test]
    fn spec_probe_ip_024_position_separates_old_packets_from_restarts() {
        assert_eq!(position(100, 100), Position::Same);
        assert_eq!(position(100, 101), Position::Forward(1));
        assert_eq!(position(0xFFFF, 0x0000), Position::Forward(1));
        assert_eq!(position(300, 50), Position::Backward(250));
        assert_eq!(position(0x0000, 0xFFFF), Position::Backward(1));
        assert_eq!(position(100, 40_000), Position::FarAway);
        // Exatamente na borda da janela ainda é a mesma sequência.
        assert_eq!(
            position(0, MAX_DROPOUT),
            Position::Forward(u32::from(MAX_DROPOUT))
        );
        assert_eq!(position(0, MAX_DROPOUT + 1), Position::FarAway);
    }

    /// SPEC-PROBE-IP-021 — a razão de perda usa `recebidos + perdidos` como
    /// denominador, que é o que dá sentido ao limiar de 1e-4 do §8.
    #[test]
    fn spec_probe_ip_021_loss_ratio_is_over_expected_packets() {
        let (mut s, t0) = state();
        for seq in 0..999u16 {
            s.observe(seq, t0);
        }
        s.observe(1_000, t0); // pula o 999
        s.expire(t0 + Duration::from_secs(1), WINDOW);

        let c = s.counters();
        assert_eq!(c.missing_confirmed, 1);
        assert_eq!(c.received, 1_000);
        let ratio = c.loss_ratio().expect("com pacotes há razão");
        assert!((ratio - 1.0 / 1001.0).abs() < 1e-9, "{ratio}");

        // Sem pacote nenhum, "sem dado" — nunca 0 %, que significaria "medido".
        assert_eq!(RtpSeqCounters::default().loss_ratio(), None);
    }

    /// §7 — uma rajada de perda dentro de `MAX_DROPOUT` continua sendo perda,
    /// e não reinício de fonte.
    #[test]
    fn spec_probe_ip_021_burst_below_max_dropout_is_loss() {
        let (mut s, t0) = state();
        s.observe(1_000, t0);
        assert_eq!(s.observe(1_130, t0), SeqOutcome::Gap { missing: 129 });
        assert_eq!(s.expire(t0 + Duration::from_millis(300), WINDOW), 129);
        assert_eq!(s.counters().source_restarts, 0);
    }
}
