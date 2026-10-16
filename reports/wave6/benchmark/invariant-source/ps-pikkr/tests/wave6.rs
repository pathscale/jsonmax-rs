extern crate pikkr;

use pikkr::avx;
use pikkr::index_builder::{build_string_mask_bitmap, build_structural_character_bitmap, build_structural_quote_bitmap};
use pikkr::Pikkr;

fn raw_masks(input: &[u8]) -> [Vec<u64>; 5] {
    let mut masks = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let (a, rest) = masks.split_at_mut(1);
    let (b, rest) = rest.split_at_mut(1);
    let (c, rest) = rest.split_at_mut(1);
    let (d, e) = rest.split_at_mut(1);
    build_structural_character_bitmap(
        input,
        &mut a[0],
        &mut b[0],
        &mut c[0],
        &mut d[0],
        &mut e[0],
        &avx::mm256i(b'\\' as i8),
        &avx::mm256i(b'"' as i8),
        &avx::mm256i(b':' as i8),
        &avx::mm256i(b'{' as i8),
        &avx::mm256i(b'}' as i8),
    );
    masks
}

fn check_masks(input: &[u8]) {
    let masks = raw_masks(input);
    let needles = [b'\\', b'"', b':', b'{', b'}'];
    for (mask, needle) in masks.iter().zip(needles.iter()) {
        let expected: Vec<u64> = input.chunks(64).map(|chunk| chunk.iter().enumerate().fold(0, |m, (i, byte)| m | if byte == needle { 1 << i } else { 0 })).collect();
        assert_eq!(*mask, expected, "raw byte {} length {}", needle, input.len());
    }
    let mut quotes = masks[1].clone();
    build_structural_quote_bitmap(&masks[0], &mut quotes);
    let mut outside = Vec::new();
    build_string_mask_bitmap(&quotes, &mut outside);
    let mut slash_run = 0;
    let mut inside = false;
    for (i, &byte) in input.iter().enumerate() {
        let delimiter = byte == b'"' && slash_run % 2 == 0;
        assert_eq!((quotes[i / 64] >> (i % 64)) & 1 != 0, delimiter, "delimiter at {} in {:?}", i, input);
        // Upstream outside mask includes opening quotes, excludes closing quotes.
        assert_eq!((outside[i / 64] >> (i % 64)) & 1 != 0, !inside, "string parity at {}", i);
        if delimiter {
            inside = !inside;
        }
        slash_run = if byte == b'\\' { slash_run + 1 } else { 0 };
    }
}

#[test]
fn every_tail_and_structural_position_matches_scalar() {
    for full_blocks in 0..=3 {
        for tail in 0..=64 {
            let len = full_blocks * 64 + tail;
            let mut input = vec![0xff; len];
            check_masks(&input);
            for pos in 0..len {
                for &byte in b"\\\":{}" {
                    input[pos] = byte;
                    check_masks(&input);
                }
                input[pos] = 0xff;
            }
        }
    }
}

#[test]
fn backslash_quote_runs_across_lanes_and_multiple_blocks() {
    for start in [0, 1, 14, 15, 16, 30, 31, 32, 46, 47, 48, 62, 63, 64, 65].iter().cloned() {
        for run in 0..=193 {
            let mut input = vec![b'x'; start];
            input.extend(std::iter::repeat(b'\\').take(run));
            input.extend_from_slice(b"\":{\"inside\\\"{}\":\"end\"}");
            check_masks(&input);
        }
    }
    let mut state = 0xcafe_1234_5678_90abu64;
    for len in 0..=512 {
        let mut input = Vec::with_capacity(len);
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            input.push(b"\\\\\\\"\"{}:abc\xff"[(state as usize) % 12]);
        }
        check_masks(&input);
    }
}

#[test]
fn records_at_every_tail_trained_untrained_and_reordered() {
    let queries = ["$.a", "$.nested.x", "$.arr", "$.missing"];
    let records = [r#"{"a":"é中😀","nested":{"x":42},"arr":[1,{"x":"y"},[2,3]]}"#, r#"{"arr":[1,{"x":"y"},[2,3]],"nested":{"x":42},"a":"é中😀"}"#];
    let expected = vec![Some("\"é中😀\"".as_bytes()), Some(&b"42"[..]), Some(&br#"[1,{"x":"y"},[2,3]]"#[..]), None];
    for train in [0, 1, 2, usize::MAX].iter().cloned() {
        let mut parser = Pikkr::new(&queries, train).unwrap();
        for tail in 0..=64 {
            for record in &records {
                let mut input = record.to_string();
                input.extend(std::iter::repeat(' ').take(tail));
                assert_eq!(parser.parse(input.as_bytes()).unwrap(), expected, "training {} padding {}", train, tail);
            }
        }
        assert_eq!(parser.parse(r#"{}"#).unwrap(), vec![None; queries.len()]);
        assert_eq!(parser.parse(r#"{"nested":{},"a":false}"#).unwrap(), vec![Some(&b"false"[..]), None, None, None]);
    }
}

#[test]
fn non_ascii_keys_and_values_at_every_block_alignment() {
    let queries = ["$.日本語", "$.é.ខ្មែរ", "$.array"];
    for train in [1, usize::MAX].iter().cloned() {
        let mut parser = Pikkr::new(&queries, train).unwrap();
        for padding in 0..=128 {
            let input = format!("{}{}", " ".repeat(padding), r#"{"日本語":"😀éខ្មែរ","é":{"ខ្មែរ":"中文"},"array":["日本語",{"é":1}]}"#);
            assert_eq!(parser.parse(input.as_bytes()).unwrap(), vec![Some("\"😀éខ្មែរ\"".as_bytes()), Some("\"中文\"".as_bytes()), Some(r#"["日本語",{"é":1}]"#.as_bytes())]);
        }
    }
}

#[test]
fn escaped_quotes_in_selected_values_across_blocks() {
    let queries = ["$.value", "$.after"];
    for train in [1, usize::MAX].iter().cloned() {
        let mut parser = Pikkr::new(&queries, train).unwrap();
        for quote_at in [16usize, 32, 48, 64, 128, 192, 256].iter().cloned() {
            for run in 1..=quote_at - 10 {
                // The run ends at quote_at - 1. Odd parity escapes the quote;
                // even parity closes the string there.
                let mut input = String::from("{\"value\":\"");
                input.push_str(&"x".repeat(quote_at - run - input.len()));
                input.push_str(&"\\".repeat(run));
                input.push('"');
                if run % 2 == 1 {
                    input.push_str("inside:{}[]é\"");
                }
                let value_end = input.len();
                input.push_str(",\"after\":7}");
                assert_eq!(parser.parse(input.as_bytes()).unwrap(), vec![Some(&input.as_bytes()[9..value_end]), Some(&b"7"[..])]);
            }
        }
    }
}

#[test]
fn partial_speculation_falls_back_for_each_field_order() {
    let queries = ["$.a", "$.b", "$.nested.x", "$.arr"];
    let mut trained = Pikkr::new(&queries, 1).unwrap();
    let mut basic = Pikkr::new(&queries, usize::MAX).unwrap();
    let training = r#"{"a":1,"b":2,"nested":{"x":3},"arr":[{"x":4}]}"#;
    trained.parse(training).unwrap();
    let fields = [r#""a":11"#, r#""b":22"#, r#""nested":{"x":33}"#, r#""arr":[{"x":44}]"#];
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    if a == b || a == c || a == d || b == c || b == d || c == d {
                        continue;
                    }
                    let input = format!("{{{},{},{},{}}}", fields[a], fields[b], fields[c], fields[d]);
                    assert_eq!(trained.parse(input.as_bytes()), basic.parse(input.as_bytes()));
                }
            }
        }
    }
}

#[test]
fn query_results_are_raw_borrowed_selection_not_validation() {
    let mut parser = Pikkr::new(&["$.selected"], 1).unwrap();
    // The unselected atom is invalid JSON; this is deliberately query scope.
    let input = br#"{"selected":"\u0061","unchecked":not_json}"#;
    let result: pikkr::Result<_> = parser.parse(input);
    let selected = result.unwrap()[0].unwrap();
    assert_eq!(selected, br#""\u0061""#);
    assert_eq!(selected.as_ptr(), input[12..].as_ptr());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn tails_at_an_unreadable_page_boundary() {
    use pikkr::index_builder::IndexBuilder;
    use std::os::raw::{c_int, c_void};
    extern "C" {
        fn getpagesize() -> c_int;
        fn mmap(addr: *mut c_void, length: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
        fn mprotect(addr: *mut c_void, length: usize, prot: c_int) -> c_int;
        fn munmap(addr: *mut c_void, length: usize) -> c_int;
    }
    struct Pages {
        ptr: *mut u8,
        len: usize,
    }
    impl Drop for Pages {
        fn drop(&mut self) {
            unsafe {
                assert_eq!(munmap(self.ptr as *mut c_void, self.len), 0);
            }
        }
    }
    unsafe {
        let page = getpagesize() as usize;
        let anonymous = if cfg!(target_os = "macos") { 0x1000 } else { 0x20 };
        let ptr = mmap(std::ptr::null_mut(), page * 2, 3, 2 | anonymous, -1, 0);
        assert_ne!(ptr as isize, -1);
        let pages = Pages { ptr: ptr as *mut u8, len: page * 2 };
        assert_eq!(mprotect(pages.ptr.add(page) as *mut c_void, page, 0), 0);
        for blocks in 0..=3 {
            for tail in 0..=64 {
                let len = blocks * 64 + tail;
                let begin = pages.ptr.add(page - len);
                for i in 0..len {
                    *begin.add(i) = b"\\\":{}\xff"[i % 6];
                }
                check_masks(std::slice::from_raw_parts(begin, len));
            }
        }
        let mut parser = Pikkr::new(&["$.a"], 1).unwrap();
        let mut index = IndexBuilder::new(1);
        for padding in 0..=64 {
            let input = format!("{{\"a\":\"中😀\",\"b\":[1,{{\"x\":2}}]}}{}", " ".repeat(padding));
            let begin = pages.ptr.add(page - input.len());
            std::ptr::copy_nonoverlapping(input.as_ptr(), begin, input.len());
            let input = std::slice::from_raw_parts(begin, input.len());
            index.build_structural_indices(input).unwrap();
            assert_eq!(parser.parse(input).unwrap(), vec![Some("\"中😀\"".as_bytes())]);
        }
    }
}
