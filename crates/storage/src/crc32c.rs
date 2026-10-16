//! CRC-32C (Castagnoli), the checksum on every page and, later, every WAL
//! record.  Chosen over CRC-32 for its better error detection on storage
//! data and because CPUs accelerate it (SSE 4.2, ARMv8 CRC); this is the
//! portable table-driven version, which a hardware path can replace without
//! changing a single stored value.

const POLY: u32 = 0x82F6_3B78; // reflected Castagnoli polynomial

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8)
    })
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
}
