//! CRC-32C (Castagnoli), the checksum on every page and every WAL record.
//! Chosen over CRC-32 for its better error detection on storage data and
//! because CPUs accelerate it: `nucleus-kernels` computes it by the CPU's
//! instructions where it has them (ARMv8 CRC, SSE 4.2), else portably —
//! the same value either way.

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    nucleus_kernels::crc32c(data)
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
                        (c >> 1) ^ 0x82F6_3B78 // the reflected Castagnoli polynomial
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
