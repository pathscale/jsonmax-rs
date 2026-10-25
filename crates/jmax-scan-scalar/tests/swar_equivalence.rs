//! The word-at-a-time classifier must match the byte-loop reference exactly.

use jmax_scan_scalar::{BLOCK_BYTES, classify, structural, tails};

fn check(block: &[u8; BLOCK_BYTES]) {
    let reference = tails::classify_scalar_raw(block);
    assert_eq!(classify::classify_raw(block), reference, "block {block:?}");
    assert_eq!(structural::classify_structural(block), reference.classes);
}

#[test]
fn every_byte_value_in_every_lane_matches_reference() {
    for position in 0..BLOCK_BYTES {
        for value in 0..=255u8 {
            for fill in [0u8, b'a', 0xff, b'"'] {
                let mut block = [fill; BLOCK_BYTES];
                block[position] = value;
                check(&block);
            }
        }
    }
}

#[test]
fn mixed_blocks_match_reference() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..20_000 {
        let mut block = [0u8; BLOCK_BYTES];
        for byte in &mut block {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        check(&block);
        // Bias toward JSON-relevant bytes as well.
        for byte in &mut block {
            const PICK: &[u8] = b"{}[]:,\" \t\r\n\\09azAZ\x00\x1f\x7f\x80\xff/.";
            *byte = PICK[(*byte as usize) % PICK.len()];
        }
        check(&block);
    }
}
