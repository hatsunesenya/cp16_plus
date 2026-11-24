//! CRC-16/CCITT-FALSE：init 0xFFFF、poly 0x1021、MSB 先出、无反射、无终异或。

pub fn crc16(bits: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bits {
        crc ^= ((b & 1) as u16) << 15;
        if crc & 0x8000 != 0 {
            crc = (crc << 1) ^ 0x1021;
        } else {
            crc <<= 1;
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // "123456789" 的 ASCII 比特流，CCITT-FALSE 应为 0x29B1
        let bits: Vec<u8> = b"123456789"
            .iter()
            .flat_map(|&c| (0..8).rev().map(move |i| (c >> i) & 1))
            .collect();
        assert_eq!(crc16(&bits), 0x29B1);
    }
}
