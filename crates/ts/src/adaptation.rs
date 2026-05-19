//! Parse de Adaptation Field MPEG-TS e conversão de PCR.
//!
//! SPEC-TS-004 · SPEC-TS-004a

use std::time::Duration;

use crate::TsError;

/// Adaptation Field de um pacote MPEG-TS (presente quando AFC = `0b10` ou `0b11`).
///
/// SPEC-TS-004
#[derive(Debug, Clone)]
pub struct AdaptationField {
    /// Flag de descontinuidade — pode indicar mudança abrupta de PCR ou fluxo.
    pub discontinuity_indicator: bool,
    /// Flag de acesso aleatório — início de GOP ou seção independente.
    pub random_access_indicator: bool,
    /// PCR — Program Clock Reference (valor de 42 bits: `base * 300 + extensão`).
    ///
    /// SPEC-TS-004
    pub pcr: Option<u64>,
    /// OPCR — Original Program Clock Reference.
    pub opcr: Option<u64>,
    /// Splice countdown (presente se `splicing_point_flag` estiver setado).
    pub splice_countdown: Option<i8>,
}

impl AdaptationField {
    /// Faz parse de um Adaptation Field a partir de `data`.
    ///
    /// SPEC-TS-004
    ///
    /// `data[0]` deve ser `adaptation_field_length`. O total de bytes consumidos
    /// é `1 + data[0]` (o byte de tamanho mais o conteúdo).
    ///
    /// # Errors
    ///
    /// Retorna [`TsError::MalformedAdaptationField`] se `data` for muito curto
    /// para o `adaptation_field_length` declarado ou para os campos opcionais.
    pub(crate) fn parse(data: &[u8]) -> Result<Self, TsError> {
        if data.is_empty() {
            return Err(TsError::MalformedAdaptationField);
        }
        let length = data[0] as usize;

        if length == 0 {
            // Apenas stuffing; sem flags nem campos opcionais.
            return Ok(AdaptationField {
                discontinuity_indicator: false,
                random_access_indicator: false,
                pcr: None,
                opcr: None,
                splice_countdown: None,
            });
        }

        // data[1..=length] é o conteúdo do adaptation field.
        let end = 1 + length; // índice exclusivo em `data`
        if data.len() < end {
            return Err(TsError::MalformedAdaptationField);
        }

        let flags = data[1];
        let discontinuity_indicator = (flags & 0x80) != 0;
        let random_access_indicator = (flags & 0x40) != 0;
        let pcr_flag = (flags & 0x10) != 0;
        let opcr_flag = (flags & 0x08) != 0;
        let splicing_flag = (flags & 0x04) != 0;

        // offset cresce conforme lemos campos opcionais; começa após o byte de flags.
        let mut offset = 2usize;

        let pcr = if pcr_flag {
            if offset + 6 > end {
                return Err(TsError::MalformedAdaptationField);
            }
            let b = &data[offset..offset + 6];
            let pcr_val = decode_pcr(b);
            offset += 6;
            Some(pcr_val)
        } else {
            None
        };

        let opcr = if opcr_flag {
            if offset + 6 > end {
                return Err(TsError::MalformedAdaptationField);
            }
            let b = &data[offset..offset + 6];
            let opcr_val = decode_pcr(b);
            offset += 6;
            Some(opcr_val)
        } else {
            None
        };

        let splice_countdown = if splicing_flag {
            if offset >= end {
                return Err(TsError::MalformedAdaptationField);
            }
            let val = data[offset] as i8;
            // offset += 1; — não precisamos avançar mais além deste campo
            Some(val)
        } else {
            None
        };

        Ok(AdaptationField {
            discontinuity_indicator,
            random_access_indicator,
            pcr,
            opcr,
            splice_countdown,
        })
    }
}

/// Decodifica 6 bytes de PCR/OPCR no valor de 42 bits `base * 300 + ext`.
///
/// Layout (ISO/IEC 13818-1 Tabela 2-6):
/// ```text
/// b[0]      b[1]      b[2]      b[3]      b[4]        b[5]
/// pppppppp  pppppppp  pppppppp  pppppppp  pxxxxxxe    eeeeeeee
///  ↑ base 33 bits (MSB → LSB) ↑          └┤ ext 9b ├┘
/// ```
/// Onde `p` = bits do PCR base, `x` = bits reservados (ignorados), `e` = bits da extensão.
#[inline]
fn decode_pcr(b: &[u8]) -> u64 {
    let base = ((b[0] as u64) << 25)
        | ((b[1] as u64) << 17)
        | ((b[2] as u64) << 9)
        | ((b[3] as u64) << 1)
        | ((b[4] as u64) >> 7);
    let ext = (((b[4] & 0x01) as u64) << 8) | (b[5] as u64);
    base * 300 + ext
}

/// Converte um valor PCR de 27 MHz para [`Duration`].
///
/// SPEC-TS-004a
///
/// `pcr` é o valor de 42 bits: `pcr_base * 300 + pcr_ext`.
/// Frequência de referência: 27 000 000 Hz → `Duration = pcr / 27_000_000` segundos.
///
/// Precisão: nanosegundos (erro < 1 ns para valores PCR comuns).
///
/// # Overflow
///
/// PCR máximo ≈ 2^42 ≈ 4,4 × 10^12. `pcr * 1000 ≈ 4,4 × 10^15 < u64::MAX`. Sem overflow.
pub fn pcr_to_duration(pcr: u64) -> Duration {
    // pcr / 27_000_000 segundos
    // = pcr * 1_000_000_000 / 27_000_000 nanosegundos
    // = pcr * 1000 / 27 nanosegundos
    Duration::from_nanos(pcr * 1000 / 27)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pcr_to_duration ──────────────────────────────────────────────────────

    #[test]
    fn spec_ts_004_pcr_to_duration_zero() {
        assert_eq!(pcr_to_duration(0), Duration::ZERO);
    }

    /// Verifica precisão de µs: 27 ticks = 1 µs, 27 000 ticks = 1 ms, 27 000 000 ticks = 1 s.
    #[test]
    fn spec_ts_004_pcr_to_duration_precision() {
        assert_eq!(pcr_to_duration(27), Duration::from_micros(1));
        assert_eq!(pcr_to_duration(27_000), Duration::from_millis(1));
        assert_eq!(pcr_to_duration(27_000_000), Duration::from_secs(1));
    }

    // ── decode PCR ───────────────────────────────────────────────────────────

    /// Verifica que o decode de PCR de 6 bytes produz o valor esperado.
    #[test]
    fn spec_ts_004_pcr_decode_known_value() {
        // Encodar manualmente: base = 0x12345, ext = 150
        let pcr_base: u64 = 0x12345;
        let pcr_ext: u64 = 150;
        let expected = pcr_base * 300 + pcr_ext;

        // Montar os 6 bytes de PCR conforme ISO 13818-1
        let b0 = (pcr_base >> 25) as u8;
        let b1 = (pcr_base >> 17) as u8;
        let b2 = (pcr_base >> 9) as u8;
        let b3 = (pcr_base >> 1) as u8;
        // b4: [base_lsb][6 bits reservados = 0b111111][ext_msb]
        let b4 = (((pcr_base & 0x01) as u8) << 7) | 0x7E | (((pcr_ext >> 8) & 0x01) as u8);
        let b5 = (pcr_ext & 0xFF) as u8;

        // Montar adaptation field: length=7, flags=0x10 (PCR_flag), 6 bytes PCR
        let af_data = [0x07u8, 0x10, b0, b1, b2, b3, b4, b5];
        let af = AdaptationField::parse(&af_data).unwrap();
        assert_eq!(af.pcr, Some(expected));
    }

    /// adaptation_field_length = 0 → stuffing apenas, sem PCR.
    #[test]
    fn spec_ts_004_adaptation_field_stuffing_only() {
        let af_data = [0x00u8];
        let af = AdaptationField::parse(&af_data).unwrap();
        assert!(!af.discontinuity_indicator);
        assert!(!af.random_access_indicator);
        assert!(af.pcr.is_none());
    }

    /// Adaptation field truncado deve retornar erro.
    #[test]
    fn spec_ts_004_adaptation_field_truncated() {
        // Declara length=10, mas fornece apenas 4 bytes no total
        let af_data = [0x0Au8, 0x10, 0x00, 0x01]; // muito curto
        assert!(matches!(
            AdaptationField::parse(&af_data),
            Err(TsError::MalformedAdaptationField)
        ));
    }

    /// Flags de discontinuity e random_access lidos corretamente.
    #[test]
    fn spec_ts_004_adaptation_field_flags() {
        // length=1, flags = 0xC0 (discontinuity=1, random_access=1)
        let af_data = [0x01u8, 0xC0];
        let af = AdaptationField::parse(&af_data).unwrap();
        assert!(af.discontinuity_indicator);
        assert!(af.random_access_indicator);
        assert!(af.pcr.is_none());
    }
}
