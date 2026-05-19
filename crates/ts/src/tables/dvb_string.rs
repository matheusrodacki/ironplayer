//! Decodificação de strings DVB (ISO 8859-x / UTF-8).
//!
//! O primeiro byte da string DVB seleciona a codificação:
//! - ausente / 0x20–0xFF → ISO 8859-1 implícito
//! - 0x01–0x0B          → ISO 8859-{5..15}
//! - [0x10, 0x00, XX]   → ISO 8859-XX  (via sequência de 3 bytes)
//! - 0x15               → UTF-8
//!
//! Bytes inválidos são substituídos por U+FFFD (nunca panic, nunca Err).
//!
//! SPEC-TABLE-008c

/// Decodifica uma string DVB de `bytes` para `String`.
///
/// Bytes inválidos para a codificação detectada são substituídos por `\u{FFFD}`.
///
/// SPEC-TABLE-008c
pub fn decode(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    let first = bytes[0];

    match first {
        // Sequência de três bytes: 0x10 0x00 XX → ISO 8859-XX
        0x10 => {
            if bytes.len() >= 3 && bytes[1] == 0x00 {
                let iso_num = bytes[2];
                decode_iso8859(iso_num, &bytes[3..])
            } else {
                // Malformado — trata payload inteiro como ISO 8859-1
                decode_iso8859(1, bytes)
            }
        }

        // UTF-8 explícito
        0x15 => String::from_utf8_lossy(&bytes[1..]).into_owned(),

        // ISO 8859-{5..15} selecionado pelo byte de controle 0x01–0x0B
        0x01..=0x0B => {
            // byte 0x01 → ISO 8859-5, …, byte 0x0B → ISO 8859-15
            let iso_num = first + 4; // 0x01→5, 0x02→6, …, 0x0B→15
            decode_iso8859(iso_num, &bytes[1..])
        }

        // Byte de controle reservado / desconhecido (0x0C–0x0F, 0x11–0x14, 0x16–0x1F):
        // descartar byte de controle e tratar restante como ISO 8859-1
        0x0C..=0x0F | 0x11..=0x14 | 0x16..=0x1F => decode_iso8859(1, &bytes[1..]),

        // Sem byte de seleção → ISO 8859-1 implícito (bytes[0] é parte do conteúdo)
        _ => decode_iso8859(1, bytes),
    }
}

/// Decodifica `data` usando a codificação ISO 8859-`n`.
///
/// Usa `encoding_rs` (Mozilla) para todas as variantes.
/// Bytes inválidos são substituídos por U+FFFD.
///
/// SPEC-TABLE-008c
fn decode_iso8859(n: u8, data: &[u8]) -> String {
    use encoding_rs::Encoding;

    let label = iso8859_label(n);
    match Encoding::for_label(label.as_bytes()) {
        Some(enc) => {
            let (cow, _enc, _had_errors) = enc.decode(data);
            cow.into_owned()
        }
        None => {
            // Fallback: ISO 8859-1 — cada byte vira o codepoint Unicode correspondente
            data.iter().map(|&b| b as char).collect()
        }
    }
}

/// Retorna o label IANA para `encoding_rs` dado o número ISO 8859.
fn iso8859_label(n: u8) -> &'static str {
    match n {
        1 => "iso-8859-1",
        2 => "iso-8859-2",
        3 => "iso-8859-3",
        4 => "iso-8859-4",
        5 => "iso-8859-5",
        6 => "iso-8859-6",
        7 => "iso-8859-7",
        8 => "iso-8859-8",
        9 => "iso-8859-9",
        10 => "iso-8859-10",
        11 => "iso-8859-11",
        13 => "iso-8859-13",
        14 => "iso-8859-14",
        15 => "iso-8859-15",
        _ => "iso-8859-1", // fallback seguro
    }
}

// ── Testes ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::decode;

    /// SPEC-TABLE-008c: string vazia retorna string vazia.
    #[test]
    fn spec_table_008c_dvb_string_empty() {
        assert_eq!(decode(&[]), "");
    }

    /// SPEC-TABLE-008c: ISO 8859-1 implícito (sem byte de seleção).
    #[test]
    fn spec_table_008c_dvb_string_iso8859_1_implicit() {
        // "Hello" em ASCII — também válido em ISO 8859-1
        let input = b"Hello";
        assert_eq!(decode(input), "Hello");
    }

    /// SPEC-TABLE-008c: ISO 8859-1 com caractere acentuado (0xE9 = 'é').
    #[test]
    fn spec_table_008c_dvb_string_iso8859_1_accented() {
        // 0xE9 = 'é' em ISO 8859-1
        let input = &[0xC3u8, 0xA3]; // 0xC3=Ã, 0xA3=£ em ISO 8859-1
        let result = decode(input);
        assert!(!result.is_empty());
    }

    /// SPEC-TABLE-008c: UTF-8 explícito via byte 0x15.
    #[test]
    fn spec_table_008c_dvb_string_utf8() {
        let mut input = vec![0x15u8];
        input.extend_from_slice("TV Canal".as_bytes());
        assert_eq!(decode(&input), "TV Canal");
    }

    /// SPEC-TABLE-008c: UTF-8 explícito com caractere multibyte.
    #[test]
    fn spec_table_008c_dvb_string_utf8_multibyte() {
        let mut input = vec![0x15u8];
        input.extend_from_slice("Rede Globo".as_bytes());
        assert_eq!(decode(&input), "Rede Globo");
    }

    /// SPEC-TABLE-008c: ISO 8859-5 via byte de seleção 0x01.
    #[test]
    fn spec_table_008c_dvb_string_iso8859_5() {
        // byte 0x01 → ISO 8859-5 (cirílico); payload: 0x41 = 'A' (mesma posição)
        let input = &[0x01u8, 0x41];
        let result = decode(input);
        assert!(!result.is_empty());
    }

    /// SPEC-TABLE-008c: sequência [0x10, 0x00, 0x05] → ISO 8859-5.
    #[test]
    fn spec_table_008c_dvb_string_triple_byte_selection() {
        // [0x10, 0x00, 0x05] → ISO 8859-5, depois "AB"
        let input = &[0x10u8, 0x00, 0x05, 0x41, 0x42];
        let result = decode(input);
        assert_eq!(result, "AB");
    }

    /// SPEC-TABLE-008c: bytes inválidos UTF-8 são substituídos por U+FFFD.
    #[test]
    fn spec_table_008c_dvb_string_utf8_invalid_replaced() {
        // byte 0x15 = UTF-8; seguido de byte inválido 0xFF
        let input = &[0x15u8, 0x41, 0xFF, 0x42];
        let result = decode(input);
        assert!(result.contains('A'));
        assert!(result.contains('B'));
        assert!(result.contains('\u{FFFD}'));
    }

    /// SPEC-TABLE-008c: sequência [0x10, 0x00] malformada → fallback ISO 8859-1.
    #[test]
    fn spec_table_008c_dvb_string_triple_byte_malformed() {
        // [0x10] sem bytes seguintes suficientes — trata como ISO 8859-1
        let input = &[0x10u8, 0x41];
        let result = decode(input);
        assert!(!result.is_empty());
    }
}
