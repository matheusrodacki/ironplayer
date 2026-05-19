//! Parser da TDT (Time and Date Table).
//!
//! SPEC-TABLE-006

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, NaiveDateTime, Timelike as _};

use super::TableError;

// ── Funções de decodificação MJD + BCD ────────────────────────────────────────

/// Decodifica um par MJD+BCD em `NaiveDateTime` UTC.
///
/// Retorna `None` se os bytes BCD forem inválidos (ex: `HH=0xFF`).
///
/// **Algoritmo MJD (EN 300 468 Annex C):**
/// ```text
/// Y = INT((MJD – 15078.2) / 365.25)
/// M = INT((MJD – 14956.1 – INT(Y × 365.25)) / 30.6001)
/// D = MJD – 14956 – INT(Y × 365.25) – INT(M × 30.6001)
/// K = if M == 14 || M == 15 { 1 } else { 0 }
/// year  = Y + K + 1900
/// month = M – 1 – K × 12
/// day   = D
/// ```
///
/// SPEC-TABLE-006
pub(crate) fn decode_mjd_bcd(
    mjd: u16,
    hh: u8,
    mm: u8,
    ss: u8,
) -> Option<NaiveDateTime> {
    let mjd_f = mjd as f64;

    let y = ((mjd_f - 15078.2) / 365.25).floor() as i64;
    let m = ((mjd_f - 14956.1 - (y as f64 * 365.25).floor()) / 30.6001).floor() as i64;
    let d = mjd as i64
        - 14956
        - (y as f64 * 365.25).floor() as i64
        - (m as f64 * 30.6001).floor() as i64;

    let k = if m == 14 || m == 15 { 1i64 } else { 0i64 };
    let year  = (y + k + 1900) as i32;
    let month = (m - 1 - k * 12) as u32;
    let day   = d as u32;

    let hour = bcd_byte(hh)?;
    let min  = bcd_byte(mm)?;
    let sec  = bcd_byte(ss)?;

    NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, min, sec)
}

/// Decodifica um byte BCD (dois dígitos de 4 bits) para `u32`.
///
/// Retorna `None` se algum nibble for > 9 (BCD inválido).
fn bcd_byte(b: u8) -> Option<u32> {
    let hi = (b >> 4) as u32;
    let lo = (b & 0x0F) as u32;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(hi * 10 + lo)
}

// ── Tdt ───────────────────────────────────────────────────────────────────────

/// Time and Date Table (TDT).
///
/// Transporta o tempo UTC atual do multiplex. É uma "short section"
/// (section_syntax_indicator=0), portanto não tem CRC-32.
///
/// SPEC-TABLE-006
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tdt {
    /// Hora UTC atual conforme sinalizada no stream.
    pub utc_time: NaiveDateTime,
}

impl Tdt {
    /// Parseia uma seção TDT bruta (8 bytes, sem CRC-32).
    ///
    /// Layout esperado:
    /// ```text
    /// [table_id=0x70 1B]
    /// [section_syntax_indicator=0 | reserved | section_length[11:8] 1B]
    /// [section_length[7:0] 1B]  = 0x05
    /// [MJD[15:8] 1B]
    /// [MJD[7:0]  1B]
    /// [BCD HH 1B]
    /// [BCD MM 1B]
    /// [BCD SS 1B]
    /// ```
    ///
    /// SPEC-TABLE-006
    pub fn parse(section: &[u8]) -> Result<Self, TableError> {
        const EXPECTED: usize = 8;
        if section.len() < EXPECTED {
            return Err(TableError::InsufficientData {
                expected: EXPECTED,
                found:    section.len(),
            });
        }

        if section[0] != 0x70 {
            return Err(TableError::WrongTableId {
                expected: 0x70,
                found:    section[0],
            });
        }

        let mjd = u16::from_be_bytes([section[3], section[4]]);
        let hh  = section[5];
        let mm  = section[6];
        let ss  = section[7];

        let utc_time = decode_mjd_bcd(mjd, hh, mm, ss)
            .ok_or(TableError::InsufficientData {
                // Re-usa InsufficientData como indicador de parse failure;
                // BCD inválido é tratado como dados malformados.
                expected: EXPECTED,
                found:    0,
            })?;

        Ok(Tdt { utc_time })
    }

    /// Retorna a diferença em segundos entre a hora TDT e o relógio do sistema.
    ///
    /// Valor positivo indica que o relógio TDT está **adiantado** em relação
    /// ao sistema; negativo indica **atrasado**.
    ///
    /// SPEC-TABLE-006
    pub fn offset_from_system(&self) -> i64 {
        let unix_epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
            .expect("data fixa; nunca falha");
        let days = self
            .utc_time
            .date()
            .signed_duration_since(unix_epoch)
            .num_days();
        let tdt_ts = days * 86400 + self.utc_time.num_seconds_from_midnight() as i64;

        let sys_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tdt_ts - sys_ts
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use chrono::Datelike as _;
    use chrono::Timelike as _;

    use super::*;

    /// Parseia a fixture `tdt.bin` e verifica data/hora decodificada.
    ///
    /// MJD 0xE502 = 58626 → 2019-05-23; BCD 14:30:45.
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_parse_tdt_fixture() {
        let data = include_bytes!("../../tests/fixtures/tdt.bin");
        assert_eq!(data.len(), 8, "fixture TDT deve ter 8 bytes");
        assert_eq!(data[0], 0x70, "table_id deve ser 0x70 (TDT)");

        let tdt = Tdt::parse(data).expect("TDT deve parsear sem erro");

        let expected = NaiveDate::from_ymd_opt(2019, 5, 23)
            .unwrap()
            .and_hms_opt(14, 30, 45)
            .unwrap();

        assert_eq!(
            tdt.utc_time, expected,
            "UTC time deve ser 2019-05-23 14:30:45"
        );
    }

    /// `offset_from_system` retorna um inteiro sem pânico.
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_offset_from_system_functional() {
        let data = include_bytes!("../../tests/fixtures/tdt.bin");
        let tdt = Tdt::parse(data).expect("TDT deve parsear");
        // A fixture usa 2019-05-23, portanto o offset vs sistema atual deve ser
        // muito negativo (passado). Apenas verificamos que não entra em pânico.
        let offset = tdt.offset_from_system();
        assert!(offset < 0, "offset para data passada deve ser negativo");
    }

    /// Decodificação MJD correta para data conhecida.
    ///
    /// MJD 58626 = 2019-05-23 (verificado pelo algoritmo EN 300 468 Annex C).
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_mjd_decode_known_date() {
        let dt = decode_mjd_bcd(58626, 0x14, 0x30, 0x45)
            .expect("deve decodificar data conhecida");

        assert_eq!(dt.year(), 2019);
        assert_eq!(dt.month(), 5);
        assert_eq!(dt.day(), 23);
        assert_eq!(dt.hour(), 14);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 45);
    }

    /// BCD inválido (ex: 0xFF) deve retornar None.
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_invalid_bcd_returns_none() {
        assert!(decode_mjd_bcd(58626, 0xFF, 0x00, 0x00).is_none());
        assert!(decode_mjd_bcd(58626, 0x00, 0xFF, 0x00).is_none());
    }

    /// table_id incorreto deve retornar WrongTableId.
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_wrong_table_id() {
        let data = [0x42u8, 0x70, 0x05, 0xE5, 0x02, 0x14, 0x30, 0x45];
        let err = Tdt::parse(&data).unwrap_err();
        assert!(matches!(
            err,
            TableError::WrongTableId { expected: 0x70, found: 0x42 }
        ));
    }

    /// Dados insuficientes retornam InsufficientData.
    ///
    /// SPEC-TABLE-006
    #[test]
    fn spec_table_006_insufficient_data() {
        let short = [0x70u8, 0x70, 0x05];
        let err = Tdt::parse(&short).unwrap_err();
        assert!(matches!(err, TableError::InsufficientData { .. }));
    }
}
