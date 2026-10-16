use jmax_scan_neon::{classify, quotes, structural, BlockMasks, ScanCarry};

fn reference_quotes(q: u64, b: u64, len: u8, carry: &mut ScanCarry) -> (u64, u64, u64) {
    let mut delimiters = 0;
    let mut escaped_quotes = 0;
    let mut content = 0;
    let mut inside = carry.prev_in_string != 0;
    let mut odd = carry.prev_escaped != 0;
    for i in 0..len {
        let bit = 1u64 << i;
        if q & bit != 0 && !odd {
            delimiters |= bit;
            inside = !inside;
        } else {
            if q & bit != 0 {
                escaped_quotes |= bit;
            }
            if inside {
                content |= bit;
            }
        }
        odd = b & bit != 0 && !odd;
    }
    carry.prev_in_string = u64::from(inside);
    carry.prev_escaped = u64::from(odd);
    (delimiters, escaped_quotes, content)
}

#[test]
fn exhaustive_short_escape_runs_and_carries() {
    for len in 0..=10u8 {
        let valid = if len == 0 { 0 } else { (1u64 << len) - 1 };
        for b in 0..=valid {
            for incoming in 0..4u64 {
                let mut carry = ScanCarry {
                    prev_in_string: incoming & 1,
                    prev_escaped: incoming >> 1,
                    utf8_tail: [0xf0, 0x90, 0x80, 0],
                    utf8_tail_len: 3,
                };
                let mut expected = carry;
                let q = valid & !b;
                let (delimiters, escaped, content) = reference_quotes(q, b, len, &mut expected);
                let actual = quotes::quote_masks(q, b, len, &mut carry).unwrap();
                assert_eq!(
                    (
                        actual.delimiter_quote_mask,
                        actual.escaped_quote_mask,
                        actual.in_string_mask
                    ),
                    (delimiters, escaped, content)
                );
                assert_eq!(carry, expected);
            }
        }
    }
}

#[test]
fn every_tail_lane_boundary_and_multiblock_backslash_run() {
    for len in 0..=64 {
        for incoming in 0..4u64 {
            let mut carry = ScanCarry {
                prev_in_string: incoming & 1,
                prev_escaped: incoming >> 1,
                ..ScanCarry::default()
            };
            let mut expected = carry;
            for turn in 0..6 {
                let mut bytes = [b'\\'; 64];
                if turn % 2 == 1 && len != 0 {
                    bytes[len - 1] = b'"';
                }
                let raw = classify::classify_raw(&bytes);
                let q =
                    reference_quotes(raw.quote_mask, raw.backslash_mask, len as u8, &mut expected);
                let masks = classify::classify(&bytes[..len], &mut carry).unwrap();
                assert_eq!(
                    (
                        masks.delimiter_quote_mask,
                        masks.quote_mask & !masks.delimiter_quote_mask,
                        masks.in_string_mask
                    ),
                    q
                );
                assert_eq!(carry, expected);
                assert_eq!(
                    masks.valid_mask,
                    if len == 64 { !0 } else { (1u64 << len) - 1 }
                );
            }
        }
    }
}

fn expected_classes(bytes: &[u8; 64]) -> BlockMasks {
    let mut m = BlockMasks::default();
    for (i, &b) in bytes.iter().enumerate() {
        let bit = 1u64 << i;
        match b {
            b'"' => m.quote_mask |= bit,
            b'\\' => m.backslash_mask |= bit,
            b':' => m.colon_mask |= bit,
            b',' => m.comma_mask |= bit,
            b'{' => m.lbrace_mask |= bit,
            b'}' => m.rbrace_mask |= bit,
            b'[' => m.lbracket_mask |= bit,
            b']' => m.rbracket_mask |= bit,
            _ => (),
        }
        if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            m.whitespace_mask |= bit;
        }
        if b < 32 {
            m.control_mask |= bit;
        }
        if b.is_ascii_digit() {
            m.digit_mask |= bit;
        }
        if b >= 128 {
            m.non_ascii_mask |= bit;
        }
    }
    m
}

#[test]
fn fused_raw_classes_at_every_byte_position() {
    for position in 0..64 {
        for byte in 0..=255u8 {
            let mut bytes = [b'x'; 64];
            bytes[position] = byte;
            let expected = expected_classes(&bytes);
            let raw = classify::classify_raw(&bytes);
            let classes = structural::classify_structural(&bytes);
            assert_eq!(raw.classes, classes);
            assert_eq!(
                (raw.quote_mask, raw.backslash_mask),
                (expected.quote_mask, expected.backslash_mask)
            );
            assert_eq!(
                (
                    classes.colon_mask,
                    classes.comma_mask,
                    classes.lbrace_mask,
                    classes.rbrace_mask,
                    classes.lbracket_mask,
                    classes.rbracket_mask,
                    classes.whitespace_mask,
                    classes.control_mask,
                    classes.digit_mask,
                    classes.non_ascii_mask
                ),
                (
                    expected.colon_mask,
                    expected.comma_mask,
                    expected.lbrace_mask,
                    expected.rbrace_mask,
                    expected.lbracket_mask,
                    expected.rbracket_mask,
                    expected.whitespace_mask,
                    expected.control_mask,
                    expected.digit_mask,
                    expected.non_ascii_mask
                )
            );
        }
    }
}

#[test]
fn full_slice_and_array_paths_agree_and_errors_preserve_carry() {
    let mut input = [0u8; 64];
    for (i, b) in input.iter_mut().enumerate() {
        *b = [b'"', b'\\', b',', 0x80, b'x'][i % 5];
    }
    let mut a = ScanCarry::default();
    let mut b = a;
    assert_eq!(
        classify::classify(&input, &mut a).unwrap(),
        classify::classify_block(&input, &mut b).unwrap()
    );
    assert_eq!(a, b);
    let before = a;
    assert!(quotes::quote_masks(1, 1, 64, &mut a).is_err());
    assert_eq!(a, before);
    assert!(classify::classify(&[0; 65], &mut a).is_err());
    assert_eq!(a, before);
}

#[test]
fn randomized_word_masks_match_independent_byte_oracle() {
    let mut seed = 0x12ab_34cd_56ef_7890u64;
    for len in 0..=64u8 {
        for _ in 0..128 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let backslashes = seed;
            seed = seed.rotate_left(23).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let quotes_mask = seed & !backslashes;
            for incoming in 0..4u64 {
                let mut carry = ScanCarry {
                    prev_in_string: incoming & 1,
                    prev_escaped: incoming >> 1,
                    ..ScanCarry::default()
                };
                let mut expected = carry;
                let q = reference_quotes(quotes_mask, backslashes, len, &mut expected);
                let actual =
                    quotes::quote_masks(quotes_mask, backslashes, len, &mut carry).unwrap();
                assert_eq!(
                    (
                        actual.delimiter_quote_mask,
                        actual.escaped_quote_mask,
                        actual.in_string_mask
                    ),
                    q
                );
                assert_eq!(carry, expected);
            }
        }
    }
}
