//! Размер JPEG без декодирования (разбор заголовка SOF).

/// Ширина и высота JPEG или `None`, если заголовок не найден.
pub fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Пустые байты и повторы 0xFF.
        if marker == 0xFF {
            i += 1;
            continue;
        }
        // Маркеры без длины.
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        // SOF0..SOF15, кроме DHT (C4), JPG (C8) и DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_size_from_sof0() {
        // SOI, APP0 (длина 4), SOF0: длина 11, точность 8, высота 0x0500 (1280), ширина 0x0230 (560).
        let data = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x05, 0x00, 0x02, 0x30, 0x01,
            0x01, 0x11, 0x00,
        ];
        assert_eq!(jpeg_size(&data), Some((560, 1280)));
    }

    #[test]
    fn rejects_non_jpeg() {
        assert_eq!(jpeg_size(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]), None);
        assert_eq!(jpeg_size(&[0xFF, 0xD8]), None);
        assert_eq!(jpeg_size(&[]), None);
    }
}
