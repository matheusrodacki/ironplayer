//! Leitor de bits para RBSP H.264/HEVC.
//!
//! SPEC-MI-001

use super::error::MediaInfoError;

/// Remove emulation prevention bytes `0x000003` de um RBSP.
///
/// SPEC-MI-001
pub fn remove_emulation_prevention_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Leitor de bits big-endian sobre um buffer.
///
/// SPEC-MI-001
pub struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Cria um leitor sobre `data` já sem emulation prevention.
    ///
    /// SPEC-MI-001
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Retorna `true` se ainda há bits disponíveis.
    pub fn has_bits(&self, n: usize) -> bool {
        self.bit_pos + n <= self.data.len() * 8
    }

    /// Lê `n` bits (máx 32).
    ///
    /// SPEC-MI-001
    pub fn read_bits(&mut self, n: u32) -> Result<u32, MediaInfoError> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 || !self.has_bits(n as usize) {
            return Err(MediaInfoError::TruncatedBitstream);
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte_idx = self.bit_pos / 8;
            let bit_idx = 7 - (self.bit_pos % 8);
            let bit = (self.data[byte_idx] >> bit_idx) & 1;
            value = (value << 1) | u32::from(bit);
            self.bit_pos += 1;
        }
        Ok(value)
    }

    /// Lê um bit único.
    pub fn read_bit(&mut self) -> Result<u32, MediaInfoError> {
        self.read_bits(1)
    }

    /// Exp-Golomb unsigned.
    ///
    /// SPEC-MI-001
    pub fn read_ue(&mut self) -> Result<u32, MediaInfoError> {
        let mut leading_zeros = 0u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(MediaInfoError::TruncatedBitstream);
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Exp-Golomb signed.
    ///
    /// SPEC-MI-001
    pub fn read_se(&mut self) -> Result<i32, MediaInfoError> {
        let code = self.read_ue()?;
        let signed = if code % 2 == 0 {
            -(code as i32 / 2)
        } else {
            (code as i32 + 1) / 2
        };
        Ok(signed)
    }

    /// Avança `n` bits sem ler.
    pub fn skip_bits(&mut self, n: u32) -> Result<(), MediaInfoError> {
        if !self.has_bits(n as usize) {
            return Err(MediaInfoError::TruncatedBitstream);
        }
        self.bit_pos += n as usize;
        Ok(())
    }
}

/// Localiza NAL units por start code `0x000001` ou `0x00000001`.
///
/// SPEC-MI-001
pub fn find_nal_units(data: &[u8]) -> Vec<(usize, usize)> {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        let start = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(i + 3)
        } else if i + 4 < data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            Some(i + 4)
        } else {
            None
        };
        if let Some(nal_start) = start {
            let mut end = nal_start;
            while end + 2 < data.len() {
                if data[end] == 0
                    && data[end + 1] == 0
                    && (data[end + 2] == 1
                        || (end + 3 < data.len() && data[end + 2] == 0 && data[end + 3] == 1))
                {
                    break;
                }
                end += 1;
            }
            if end + 2 >= data.len() {
                end = data.len();
            }
            if end > nal_start {
                units.push((nal_start, end));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_mi_001_remove_emulation_prevention() {
        let raw = [0x00, 0x00, 0x03, 0x01, 0x02];
        let cleaned = remove_emulation_prevention_bytes(&raw);
        assert_eq!(cleaned, vec![0x00, 0x00, 0x01, 0x02]);
    }

    #[test]
    fn spec_mi_001_read_ue_zero() {
        // ue(0) = 1
        let data = [0x80]; // bit 1
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_ue().unwrap(), 0);
    }

    #[test]
    fn spec_mi_001_read_ue_one() {
        // ue(1) = 010
        let data = [0x40];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_ue().unwrap(), 1);
    }

    #[test]
    fn spec_mi_001_find_nal_units() {
        let data = [0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x01, 0x68, 0xCE];
        let nals = find_nal_units(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(data[nals[0].0], 0x67);
        assert_eq!(data[nals[1].0], 0x68);
    }
}
