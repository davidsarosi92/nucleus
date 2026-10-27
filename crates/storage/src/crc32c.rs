//! CRC-32C (Castagnoli), the checksum on every page and, later, every WAL
//! record.  Chosen over CRC-32 for its better error detection on storage
//! data and because CPUs accelerate it (SSE 4.2, ARMv8 CRC); this is the
//! portable version (slicing-by-8, eight bytes a step), which a hardware
//! path can replace without changing a single stored value.

const POLY: u32 = 0x82F6_3B78; // reflected Castagnoli polynomial

/// Slicing-by-8: `TABLES[k][b]` is the CRC of byte `b` followed by `k`
/// zero bytes, so eight bytes go in per step.
const TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut c = !0u32;
    let (chunks, rest) = data.as_chunks::<8>();
    for b in chunks {
        let lo = c ^ u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][b[4] as usize]
            ^ t[2][b[5] as usize]
            ^ t[1][b[6] as usize]
            ^ t[0][b[7] as usize];
    }
    for &b in rest {
        c = t[0][((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    #[test]
    fn check_values() {
        // RFC 3720 (iSCSI), appendix B.4, and the standard "check" value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn slicing_agrees_with_bytewise() {
        // The definition, a byte at a time, for every length up to 64.
        let bytewise = |data: &[u8]| {
            let mut c = !0u32;
            for &b in data {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        (c >> 1) ^ super::POLY
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        };
        let data: Vec<u8> = (0..64u32).map(|i| (i * 37 + 11) as u8).collect();
        for n in 0..=64 {
            assert_eq!(crc32c(&data[..n]), bytewise(&data[..n]), "length {n}");
        }
    }
}
