//! CRC-32 MPEG-2 com tabela pré-computada.
//!
//! SPEC-TS-003b
//!
//! Polinômio: `0x04C11DB7` (MSB-first / big-endian).
//! Difere do CRC-32 Ethernet (que usa reflexão de bits); não use `crc32fast`.

/// Tabela CRC-32 MPEG-2 pré-computada (polinômio `0x04C11DB7`, MSB-first).
///
/// SPEC-TS-003b
static CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut j = 0u32;
        while j < 8 {
            if crc & 0x8000_0000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Calcula CRC-32 MPEG-2 sobre `data`.
///
/// SPEC-TS-003b
///
/// Valor inicial: `0xFFFF_FFFF`. Sem XOR final (sem inversão de saída).
/// Para verificar uma seção PSI/SI completa (dados + CRC), use [`verify_crc32_mpeg2`].
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let pos = (((crc >> 24) as u8) ^ byte) as usize;
        crc = (crc << 8) ^ CRC32_TABLE[pos];
    }
    crc
}

/// Verifica CRC-32 MPEG-2 de uma seção PSI/SI completa (dados + 4 bytes de CRC no final).
///
/// SPEC-TS-003b
///
/// Retorna `true` se o CRC é válido (residual == 0 após processar dados + CRC).
pub fn verify_crc32_mpeg2(data_with_crc: &[u8]) -> bool {
    crc32_mpeg2(data_with_crc) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vetor CRC-32 MPEG-2: roundtrip com seção PAT mínima.
    ///
    /// Computa CRC sobre os dados, anexa como big-endian e verifica que o resultado
    /// de `verify_crc32_mpeg2` é `true` (residual == 0).
    #[test]
    fn spec_ts_crc32_known_vector() {
        // Seção PAT mínima (sem CRC):
        //   table_id=0x00, section_syntax_indicator=1, section_length=0x00D (13 bytes)
        //   transport_stream_id=0x0001, version=0, current_next=1
        //   section_number=0, last_section_number=0
        //   program_number=1, program_map_pid=0x0020
        let data: &[u8] = &[
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00,
            0x00, 0x01, 0xE0, 0x20,
        ];
        let crc = crc32_mpeg2(data);
        // Anexar CRC big-endian e verificar roundtrip
        let mut with_crc = data.to_vec();
        with_crc.extend_from_slice(&crc.to_be_bytes());
        assert!(
            verify_crc32_mpeg2(&with_crc),
            "roundtrip CRC-32 MPEG-2 falhou; crc calculado = 0x{:08X}",
            crc
        );
    }

    /// CRC corrompido deve ser rejeitado.
    #[test]
    fn spec_ts_crc32_corrupted_rejected() {
        let data: &[u8] = &[
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00,
            0x00, 0x01, 0xE0, 0x20,
        ];
        let crc = crc32_mpeg2(data);
        let mut with_crc = data.to_vec();
        with_crc.extend_from_slice(&crc.to_be_bytes());
        // Corromper último byte do CRC
        *with_crc.last_mut().unwrap() ^= 0xFF;
        assert!(
            !verify_crc32_mpeg2(&with_crc),
            "CRC corrompido deveria ser rejeitado"
        );
    }

    /// Slice vazio deve retornar o valor inicial 0xFFFF_FFFF.
    #[test]
    fn spec_ts_crc32_empty_slice() {
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }
}
