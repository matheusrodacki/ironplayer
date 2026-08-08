//! FEC SMPTE ST 2022-1 (header RFC 2733).
//!
//! A convenção confirmada com a operação é **`base+2` (coluna)** e **`base+4`
//! (linha)**: para um feed em 50000, as portas de FEC são 50002 e 50004.  Como
//! é a convenção do próprio ST 2022-1, `auto` deriva as portas do feed e o caso
//! normal não precisa de configuração explícita.
//!
//! **v1 não recupera pacotes** — apenas valida e mede.  A estimativa "esta
//! perda teria sido recuperável pela matriz observada" exige bufferizar L×D
//! pacotes e é a fase 3 do faseamento (§9).
//!
//! > Nota de precisão (§5.5): os offsets abaixo vêm do RFC 2733, do qual o
//! > ST 2022-1 deriva.  Enquanto não forem conferidos contra a norma, a
//! > severidade máxima dos checks de FEC é `warning`.
//!
//! SPEC-PROBE-IP-030 … SPEC-PROBE-IP-037

/// Tamanho do header FEC que segue o header RTP.
pub const FEC_HEADER_LEN: usize = 16;

/// Eixo de proteção de um fluxo de FEC.
///
/// SPEC-PROBE-IP-032
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecAxis {
    /// `D = 0` — proteção por coluna: `L = offset`, `D = NA`.
    Column,
    /// `D = 1` — proteção por linha: `offset = 1`, `L = NA`.
    Row,
}

impl FecAxis {
    /// Rótulo estável para log, CSV e painel.
    pub fn label(self) -> &'static str {
        match self {
            Self::Column => "coluna",
            Self::Row => "linha",
        }
    }
}

/// Header FEC de 16 bytes (RFC 2733 §3.2).
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |            SN base            |        length recovery        |
/// |E|  PT recovery  |                 mask                        |
/// |                          TS recovery                          |
/// |X|D|type |index|    offset     |       NA      |SNBase ext bits|
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
///
/// SPEC-PROBE-IP-031
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecHeader {
    pub sn_base: u16,
    pub length_recovery: u16,
    pub extension: bool,
    pub pt_recovery: u8,
    pub mask: u32,
    pub ts_recovery: u32,
    /// Bit X do segundo bloco.
    pub x: bool,
    /// Bit D: `false` = coluna, `true` = linha.
    pub d: bool,
    pub fec_type: u8,
    pub index: u8,
    pub offset: u8,
    /// Number of Associated packets.
    pub na: u8,
    pub sn_base_ext: u8,
}

impl FecHeader {
    /// Faz o parse dos 16 bytes que seguem o header RTP.
    ///
    /// SPEC-PROBE-IP-031 — devolve `None` em datagrama curto; dado de rede
    /// nunca faz `panic`.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < FEC_HEADER_LEN {
            return None;
        }
        Some(Self {
            sn_base: u16::from_be_bytes([data[0], data[1]]),
            length_recovery: u16::from_be_bytes([data[2], data[3]]),
            extension: data[4] & 0x80 != 0,
            pt_recovery: data[4] & 0x7F,
            mask: u32::from_be_bytes([0, data[5], data[6], data[7]]),
            ts_recovery: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            x: data[12] & 0x80 != 0,
            d: data[12] & 0x40 != 0,
            fec_type: (data[12] >> 3) & 0x07,
            index: data[12] & 0x07,
            offset: data[13],
            na: data[14],
            sn_base_ext: data[15],
        })
    }

    /// Eixo protegido por este fluxo.
    ///
    /// SPEC-PROBE-IP-032
    pub fn axis(&self) -> FecAxis {
        if self.d {
            FecAxis::Row
        } else {
            FecAxis::Column
        }
    }

    /// Dimensão da matriz que **este** fluxo revela.
    ///
    /// SPEC-PROBE-IP-032 — coluna (`D=0`) revela `L = offset` e `D = NA`; linha
    /// (`D=1`) revela `L = NA` e tem `offset = 1`.  Cada fluxo conhece só uma
    /// parte, e é por isso que a matriz completa só sai com os dois.
    pub fn matrix_hint(&self) -> FecMatrixHint {
        match self.axis() {
            FecAxis::Column => FecMatrixHint {
                l: Some(u32::from(self.offset)),
                d: Some(u32::from(self.na)),
            },
            FecAxis::Row => FecMatrixHint {
                l: Some(u32::from(self.na)),
                d: None,
            },
        }
    }
}

/// O que um fluxo de FEC diz sobre a matriz.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FecMatrixHint {
    pub l: Option<u32>,
    pub d: Option<u32>,
}

/// Matriz L×D observada, montada a partir de um ou dois fluxos.
///
/// SPEC-PROBE-IP-032 · SPEC-PROBE-IP-033 · SPEC-PROBE-IP-034
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FecMatrix {
    pub l: Option<u32>,
    pub d: Option<u32>,
}

impl FecMatrix {
    /// Incorpora o que um fluxo revelou, sem apagar o que já se sabia.
    pub fn absorb(&mut self, hint: FecMatrixHint) {
        if let Some(l) = hint.l {
            self.l = Some(l);
        }
        if let Some(d) = hint.d {
            self.d = Some(d);
        }
    }

    /// Produto L×D, quando as duas dimensões são conhecidas.
    ///
    /// SPEC-PROBE-IP-034
    pub fn lxd(&self) -> Option<u32> {
        Some(self.l? * self.d?)
    }

    /// Rótulo `L×D` do painel; `—` no lugar da dimensão desconhecida.
    pub fn label(&self) -> String {
        let fmt = |v: Option<u32>| v.map_or("—".to_string(), |n| n.to_string());
        format!("{}×{}", fmt(self.l), fmt(self.d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monta um header FEC com os campos que a spec exige ler.
    fn header(d: bool, offset: u8, na: u8) -> [u8; FEC_HEADER_LEN] {
        let mut h = [0u8; FEC_HEADER_LEN];
        h[0..2].copy_from_slice(&1_000u16.to_be_bytes()); // SN base
        h[2..4].copy_from_slice(&1_316u16.to_be_bytes()); // length recovery
        h[4] = 0x21; // E=0, PT recovery = 33
        h[5..8].copy_from_slice(&[0x00, 0x00, 0x07]); // mask
        h[8..12].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // TS recovery
        h[12] = if d { 0x40 } else { 0x00 };
        h[13] = offset;
        h[14] = na;
        h[15] = 0;
        h
    }

    /// SPEC-PROBE-IP-031 — todos os campos do header de 16 bytes saem do parse.
    #[test]
    fn spec_probe_ip_031_parses_every_fec_header_field() {
        let h = FecHeader::parse(&header(false, 8, 5)).expect("header válido");
        assert_eq!(h.sn_base, 1_000);
        assert_eq!(h.length_recovery, 1_316);
        assert!(!h.extension);
        assert_eq!(h.pt_recovery, 33);
        assert_eq!(h.mask, 7);
        assert_eq!(h.ts_recovery, 0x0001_0000);
        assert!(!h.d);
        assert_eq!(h.offset, 8);
        assert_eq!(h.na, 5);
        assert_eq!(h.index, 0);
        assert_eq!(h.sn_base_ext, 0);

        // Truncado devolve `None`, nunca `panic`.
        assert!(FecHeader::parse(&[0u8; 15]).is_none());
        assert!(FecHeader::parse(&[]).is_none());
    }

    /// SPEC-PROBE-IP-032 — coluna revela L e D; linha revela só L.
    #[test]
    fn spec_probe_ip_032_derives_l_and_d_from_each_axis() {
        let column = FecHeader::parse(&header(false, 8, 5)).expect("coluna");
        assert_eq!(column.axis(), FecAxis::Column);
        assert_eq!(
            column.matrix_hint(),
            FecMatrixHint {
                l: Some(8),
                d: Some(5)
            }
        );

        let row = FecHeader::parse(&header(true, 1, 8)).expect("linha");
        assert_eq!(row.axis(), FecAxis::Row);
        assert_eq!(row.axis().label(), "linha");
        assert_eq!(
            row.matrix_hint(),
            FecMatrixHint {
                l: Some(8),
                d: None
            }
        );
    }

    /// SPEC-PROBE-IP-032 · SPEC-PROBE-IP-034 — a matriz se completa com os dois
    /// fluxos e o produto só existe quando as duas dimensões existem.
    #[test]
    fn spec_probe_ip_034_matrix_completes_from_both_streams() {
        let mut m = FecMatrix::default();
        assert_eq!(m.lxd(), None);
        assert_eq!(m.label(), "—×—");

        m.absorb(
            FecHeader::parse(&header(true, 1, 8))
                .expect("linha")
                .matrix_hint(),
        );
        assert_eq!(m.l, Some(8));
        assert_eq!(m.lxd(), None, "sem o fluxo de coluna, D é desconhecido");

        m.absorb(
            FecHeader::parse(&header(false, 8, 5))
                .expect("coluna")
                .matrix_hint(),
        );
        assert_eq!(m.lxd(), Some(40), "L=8 · D=5 ⇒ 40, dentro do perfil");
        assert_eq!(m.label(), "8×5");

        // §7 — offset 20 com NA 8 estoura o teto de 100.
        let mut big = FecMatrix::default();
        big.absorb(
            FecHeader::parse(&header(false, 20, 8))
                .expect("coluna")
                .matrix_hint(),
        );
        assert_eq!(big.lxd(), Some(160));
    }
}
