//! IEEE CRC-32 (Castagnoli is faster but IEEE matches many disk formats).

const TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xff) as usize;
        crc = TABLE[idx] ^ (crc >> 8);
    }
    crc
}
