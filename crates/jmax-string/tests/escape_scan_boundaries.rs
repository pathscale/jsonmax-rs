//! The inlined scanner accepts arbitrary bytes and reports the exact boundary.
use jmax_string::scan::first_special;

#[test]
fn bounded_simd_and_swar_match_scalar_for_every_byte_and_lane() {
    for length in 0..=65 {
        for position in 0..length {
            for byte in 0..=255u8 {
                let mut bytes = vec![b'a'; length];
                bytes[position] = byte;
                let expected = bytes
                    .iter()
                    .position(|&b| b == b'"' || b == b'\\' || b < 0x20);
                assert_eq!(
                    first_special(&bytes),
                    expected,
                    "len {length}, lane {position}, byte {byte}"
                );
            }
        }
    }
}

#[test]
fn high_bytes_before_boundaries_do_not_hide_or_create_matches() {
    for prefix in 0..48 {
        for boundary in [b'"', b'\\', 0, 0x1f] {
            let mut bytes = vec![0xff; prefix];
            bytes.push(boundary);
            bytes.extend_from_slice(&[0x80; 20]);
            assert_eq!(first_special(&bytes), Some(prefix));
        }
    }
}
