use jmax_dom::{Limits, Payload, build};
use jmax_encode::{
    ByteSink, EncodeLimits, Error, ErrorCode, ErrorSource, JsonWriter, LimitedSink, Result,
    WriterScratch, escaped_string_len, raw, typed,
};

#[derive(Default)]
struct RecordingSink {
    bytes: Vec<u8>,
    reservations: Vec<(usize, usize)>,
    writes: Vec<Vec<u8>>,
    fail_reservation: Option<usize>,
    fail_write: Option<usize>,
}

fn external_error() -> Error {
    Error::new(
        ErrorCode::Cancelled,
        ErrorSource::Transport,
        17,
        "test sink failed",
    )
    .with_schema(7, 9)
}

impl ByteSink for RecordingSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        self.reservations.push((self.bytes.len(), additional));
        if self.fail_reservation == Some(self.reservations.len()) {
            return Err(external_error());
        }
        self.bytes
            .try_reserve(additional)
            .map_err(|_| external_error())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.writes.push(bytes.to_vec());
        if self.fail_write == Some(self.writes.len()) {
            // External sinks may append a prefix before reporting an error.
            self.bytes.extend_from_slice(&bytes[..bytes.len().min(1)]);
            return Err(external_error());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

fn reference_string(text: &str) -> Vec<u8> {
    let mut output = vec![b'"'];
    for &byte in text.as_bytes() {
        match byte {
            b'"' => output.extend_from_slice(b"\\\""),
            b'\\' => output.extend_from_slice(b"\\\\"),
            b'\x08' => output.extend_from_slice(b"\\b"),
            b'\x0c' => output.extend_from_slice(b"\\f"),
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            b'\t' => output.extend_from_slice(b"\\t"),
            0..=0x1f => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                output.extend_from_slice(&[
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(byte >> 4) as usize],
                    HEX[(byte & 15) as usize],
                ]);
            }
            _ => output.push(byte),
        }
    }
    output.push(b'"');
    output
}

#[test]
fn all_escape_spellings_and_block_edges_match_reference() -> Result<()> {
    let controls: String = (0u8..32).map(char::from).collect();
    for padding in 0..=130 {
        // Unicode crosses SIMD boundaries; the tail contains dense escapes.
        let text = format!("{}é🦀\"\\/{controls}end", "x".repeat(padding));
        let expected = reference_string(&text);
        assert_eq!(escaped_string_len(&text)?, expected.len());
        let mut output = Vec::new();
        let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
        writer.string(&text)?;
        assert_eq!(writer.finish()?, expected.len());
        assert_eq!(output, expected);
        raw::validate(&output, EncodeLimits::default())?;
    }
    Ok(())
}

#[test]
fn unescaped_unicode_is_one_maximal_run_and_one_reservation() -> Result<()> {
    let text = "é🦀hello".repeat(100);
    let mut output = RecordingSink::default();
    let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
    writer.string(&text)?;
    assert_eq!(writer.finish()?, text.len() + 2);
    assert_eq!(output.reservations, [(0, text.len() + 2)]);
    assert_eq!(
        output.writes,
        [b"\"".to_vec(), text.into_bytes(), b"\"".to_vec()]
    );
    Ok(())
}

#[test]
fn ordinary_runs_span_many_mask_blocks() -> Result<()> {
    let first = "aé".repeat(100);
    let last = "🦀z".repeat(100);
    let text = format!("{first}\n{last}");
    let mut output = RecordingSink::default();
    let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
    writer.string(&text)?;
    writer.finish()?;
    assert_eq!(
        output.writes,
        [
            b"\"".to_vec(),
            first.into_bytes(),
            b"\\n".to_vec(),
            last.into_bytes(),
            b"\"".to_vec(),
        ]
    );
    Ok(())
}

#[test]
fn escapes_extend_the_pending_reservation_by_exact_growth() -> Result<()> {
    let mut output = RecordingSink::default();
    let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
    writer.string("\n\0")?;
    assert_eq!(writer.finish()?, 10);
    // ByteSink reservations are relative to the current length. Absolute
    // endpoints grow from the minimum 4 to 5 (+1), then 10 (+5).
    assert_eq!(output.reservations, [(0, 4), (1, 4), (3, 7)]);
    assert_eq!(output.bytes, b"\"\\n\\u0000\"");
    Ok(())
}

#[test]
fn every_output_limit_is_inclusive_and_failure_poisoning_is_sticky() -> Result<()> {
    for text in ["", "abc", "é🦀", "\"\\\n\t\0\u{1f}"] {
        let expected = reference_string(text);
        for maximum in 0..=expected.len() + 1 {
            let mut output = b"prefix".to_vec();
            let limits = EncodeLimits {
                max_output_bytes: maximum,
                ..EncodeLimits::default()
            };
            let mut writer = JsonWriter::new(&mut output, limits);
            let result = writer.string(text);
            if maximum >= expected.len() {
                result?;
                assert_eq!(writer.finish()?, expected.len());
                assert_eq!(&output[6..], expected);
            } else {
                assert_eq!(
                    result.err().map(|error| error.code),
                    Some(ErrorCode::PayloadTooLarge)
                );
                assert_eq!(
                    writer.null().err().map(|error| error.code),
                    Some(ErrorCode::InternalError)
                );
                assert_eq!(
                    writer.finish().err().map(|error| error.code),
                    Some(ErrorCode::InternalError)
                );
                assert!(output.len() - 6 <= maximum);
            }
            assert_eq!(&output[..6], b"prefix");
        }
    }
    Ok(())
}

#[test]
fn escaped_keys_include_commas_colons_and_pending_suffix_in_budget() -> Result<()> {
    let expected = b"{\"\\n\":null,\"\\u0000\":true}";
    for maximum in 0..=expected.len() {
        let mut output = Vec::new();
        let limits = EncodeLimits {
            max_output_bytes: maximum,
            ..EncodeLimits::default()
        };
        let mut writer = JsonWriter::new(&mut output, limits);
        let result = (|| -> Result<()> {
            writer.begin_object()?;
            writer.key("\n")?;
            writer.null()?;
            writer.key("\0")?;
            writer.boolean(true)?;
            writer.end_object()
        })();
        if maximum == expected.len() {
            result?;
            assert_eq!(writer.finish()?, expected.len());
            assert_eq!(output, expected);
        } else {
            assert_eq!(
                result.err().map(|error| error.code),
                Some(ErrorCode::PayloadTooLarge)
            );
            assert!(writer.finish().is_err());
            assert!(output.len() <= maximum);
        }
    }
    Ok(())
}

#[test]
fn external_reserve_and_partial_write_errors_are_preserved_and_poison() -> Result<()> {
    for reserve_failure in [true, false] {
        let mut output = RecordingSink {
            fail_reservation: reserve_failure.then_some(2),
            fail_write: (!reserve_failure).then_some(3),
            ..RecordingSink::default()
        };
        let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
        assert_eq!(writer.string("safe\nend"), Err(external_error()));
        assert_eq!(
            writer.null().err().map(|error| error.code),
            Some(ErrorCode::InternalError)
        );
        assert!(writer.finish().is_err());
        assert!(output.bytes.starts_with(b"\""));
    }
    Ok(())
}

#[test]
fn owned_borrowed_and_legacy_encoders_agree_on_nested_variable_width_tape() -> Result<()> {
    let input =
        br#" {"\u0061":[{},[],{"\u00e9":"\uD83E\uDD80\n","n":-0.00E+02}],"x":true,"empty":""} "#;
    let expected = "{\"a\":[{},[],{\"é\":\"🦀\\n\",\"n\":-0.00E+02}],\"x\":true,\"empty\":\"\"}";
    let owned = build::parse(Payload::from_slice(input), Limits::strict_default())?;
    let borrowed = build::parse_borrowed(input, Limits::strict_default())?;
    let legacy = owned.to_legacy()?;
    let limits = EncodeLimits {
        max_output_bytes: expected.len(),
        ..EncodeLimits::default()
    };
    let mut owned_output = b"prefix".to_vec();
    let mut borrowed_output = Vec::new();
    let mut legacy_output = Vec::new();
    assert_eq!(
        typed::write_tape_document(&owned, &mut owned_output, limits)?,
        expected.len()
    );
    assert_eq!(
        typed::write_borrowed_document(&borrowed, &mut borrowed_output, limits)?,
        expected.len()
    );
    assert_eq!(
        typed::write_document(&legacy, &mut legacy_output, limits)?,
        expected.len()
    );
    assert_eq!(&owned_output[6..], expected.as_bytes());
    assert_eq!(borrowed_output, expected.as_bytes());
    assert_eq!(legacy_output, borrowed_output);
    let reparsed = build::parse_borrowed(&borrowed_output, Limits::strict_default())?;
    assert_eq!(reparsed.node_count(), borrowed.node_count());
    assert_eq!(reparsed.max_depth(), borrowed.max_depth());
    Ok(())
}

#[test]
fn native_tokens_write_whole_quoted_slices_with_one_output_reservation() -> Result<()> {
    let input = r#" {"plain":"value/é","é":"🦀","n":-0.00E+02,"esc\u0061ped":"a\n\u0000\"\\\/"} "#;
    let expected = r#"{"plain":"value/é","é":"🦀","n":-0.00E+02,"escaped":"a\n\u0000\"\\/"}"#;
    let owned = build::parse(
        Payload::from_slice(input.as_bytes()),
        Limits::strict_default(),
    )?;
    let borrowed = build::parse_borrowed(input.as_bytes(), Limits::strict_default())?;
    for maximum in [expected.len(), input.len()] {
        let limits = EncodeLimits {
            max_output_bytes: maximum,
            ..EncodeLimits::default()
        };
        for use_borrowed in [false, true] {
            let mut output = RecordingSink::default();
            let written = if use_borrowed {
                typed::write_borrowed_document(&borrowed, &mut output, limits)?
            } else {
                typed::write_tape_document(&owned, &mut output, limits)?
            };
            assert_eq!(written, expected.len());
            assert_eq!(output.bytes, expected.as_bytes());
            assert_eq!(output.reservations, [(0, input.len().min(maximum))]);
            // Keys and values, including extended Unicode records, arrive as
            // their original quoted token rather than three separate writes.
            for token in [
                r#""plain""#,
                r#""value/é""#,
                r#""é""#,
                r#""🦀""#,
                "-0.00E+02",
            ] {
                assert!(output.writes.iter().any(|write| write == token.as_bytes()));
            }
        }
    }
    Ok(())
}

#[test]
fn native_capacity_hints_preserve_all_output_boundaries_and_sink_prefixes() -> Result<()> {
    let input = r#" {"k":["plain","é","\u0000\n\"\\",-0e+2,{}]} "#;
    let expected = r#"{"k":["plain","é","\u0000\n\"\\",-0e+2,{}]}"#;
    let document = build::parse_borrowed(input.as_bytes(), Limits::strict_default())?;
    let mut scratch = WriterScratch::default();
    for maximum in 0..=expected.len() + 1 {
        let limits = EncodeLimits {
            max_output_bytes: maximum,
            ..EncodeLimits::default()
        };
        let mut output = b"prefix".to_vec();
        let result = typed::write_borrowed_document_with_scratch(
            &document,
            &mut output,
            limits,
            &mut scratch,
        );
        assert_eq!(&output[..6], b"prefix");
        if maximum >= expected.len() {
            assert_eq!(result?, expected.len());
            assert_eq!(&output[6..], expected.as_bytes());
        } else {
            assert_eq!(
                result.err().map(|error| error.code),
                Some(ErrorCode::PayloadTooLarge)
            );
            assert!(output.len() - 6 <= maximum);
        }
    }
    Ok(())
}

#[test]
fn native_unescaped_strings_keys_and_numbers_still_enforce_scalar_limits() -> Result<()> {
    for (input, string_limit, number_limit, succeeds) in [
        (r#""é""#, 2, 0, true),
        (r#""é""#, 1, 0, false),
        (r#"{"é":null}"#, 2, 0, true),
        (r#"{"é":null}"#, 1, 0, false),
        ("-0e+2", 0, 5, true),
        ("-0e+2", 0, 4, false),
    ] {
        let document = build::parse_borrowed(input.as_bytes(), Limits::strict_default())?;
        let limits = EncodeLimits {
            max_string_bytes: string_limit,
            max_number_bytes: number_limit,
            ..EncodeLimits::default()
        };
        let mut output = Vec::new();
        let result = typed::write_borrowed_document(&document, &mut output, limits);
        if succeeds {
            assert_eq!(result?, input.len());
            assert_eq!(output, input.as_bytes());
        } else {
            assert_eq!(
                result.err().map(|error| error.code),
                Some(ErrorCode::BudgetExhausted)
            );
        }
    }
    Ok(())
}

#[test]
fn native_upfront_reservation_failure_preserves_error_and_writer_scratch() -> Result<()> {
    let document = build::parse_borrowed(br#"{"k":[1]}"#, Limits::strict_default())?;
    let mut scratch = WriterScratch::default();
    let mut output = Vec::new();
    typed::write_borrowed_document_with_scratch(
        &document,
        &mut output,
        EncodeLimits::default(),
        &mut scratch,
    )?;
    let retained = scratch.retained_bytes();
    let mut failing = RecordingSink {
        fail_reservation: Some(1),
        ..RecordingSink::default()
    };
    assert_eq!(
        typed::write_borrowed_document_with_scratch(
            &document,
            &mut failing,
            EncodeLimits::default(),
            &mut scratch
        ),
        Err(external_error()),
    );
    assert!(failing.bytes.is_empty());
    assert_eq!(scratch.retained_bytes(), retained);
    output.clear();
    typed::write_borrowed_document_with_scratch(
        &document,
        &mut output,
        EncodeLimits::default(),
        &mut scratch,
    )?;
    assert_eq!(output, br#"{"k":[1]}"#);
    Ok(())
}

#[test]
fn scalar_and_empty_container_roots_encode_without_extra_stack() -> Result<()> {
    for input in [
        "null", "false", "true", "-0e-0", "\"é\"", "[]", "{}", "[[[[]]]]",
    ] {
        let document = build::parse_borrowed(input.as_bytes(), Limits::strict_default())?;
        let mut output = Vec::new();
        typed::write_borrowed_document(&document, &mut output, EncodeLimits::default())?;
        assert_eq!(output, input.as_bytes());
    }
    Ok(())
}

#[test]
fn native_decoded_string_limits_count_utf8_bytes_and_duplicates_keep_order() -> Result<()> {
    let document = build::parse_borrowed(br#""\u00e9""#, Limits::strict_default())?;
    let limits = EncodeLimits {
        max_depth: 0,
        max_string_bytes: 2,
        ..EncodeLimits::default()
    };
    let mut output = Vec::new();
    typed::write_borrowed_document(&document, &mut output, limits)?;
    assert_eq!(output, "\"é\"".as_bytes());
    output.clear();
    let limits = EncodeLimits {
        max_string_bytes: 1,
        ..limits
    };
    assert_eq!(
        typed::write_borrowed_document(&document, &mut output, limits)
            .err()
            .map(|error| error.code),
        Some(ErrorCode::BudgetExhausted)
    );
    assert!(output.is_empty());

    let document = build::parse_borrowed(br#"{"a":1,"\u0061":2}"#, Limits::permissive_default())?;
    typed::write_borrowed_document(&document, &mut output, EncodeLimits::default())?;
    assert_eq!(output, br#"{"a":1,"a":2}"#);
    Ok(())
}

#[test]
fn native_limits_and_scratch_recovery_after_errors() -> Result<()> {
    let input = br#"{"key":["\u00e9",123]}"#;
    let document = build::parse(Payload::from_slice(input), Limits::strict_default())?;
    let mut scratch = WriterScratch::default();
    for (limits, code) in [
        (
            EncodeLimits {
                max_depth: 1,
                ..EncodeLimits::default()
            },
            ErrorCode::DepthLimitExceeded,
        ),
        (
            EncodeLimits {
                max_container_items: 1,
                ..EncodeLimits::default()
            },
            ErrorCode::BudgetExhausted,
        ),
        (
            EncodeLimits {
                max_string_bytes: 2,
                ..EncodeLimits::default()
            },
            ErrorCode::BudgetExhausted,
        ),
        (
            EncodeLimits {
                max_number_bytes: 2,
                ..EncodeLimits::default()
            },
            ErrorCode::BudgetExhausted,
        ),
        (
            EncodeLimits {
                max_output_bytes: 8,
                ..EncodeLimits::default()
            },
            ErrorCode::PayloadTooLarge,
        ),
    ] {
        let mut output = Vec::new();
        let result =
            typed::write_tape_document_with_scratch(&document, &mut output, limits, &mut scratch);
        assert_eq!(result.err().map(|error| error.code), Some(code));
        output.clear();
        typed::write_tape_document_with_scratch(
            &document,
            &mut output,
            EncodeLimits::default(),
            &mut scratch,
        )?;
        assert_eq!(output, "{\"key\":[\"é\",123]}".as_bytes());
    }
    assert!(scratch.retained_bytes() > 0);
    let retained = scratch.retained_bytes();
    let mut failing = RecordingSink {
        fail_write: Some(2),
        ..RecordingSink::default()
    };
    assert_eq!(
        typed::write_tape_document_with_scratch(
            &document,
            &mut failing,
            EncodeLimits::default(),
            &mut scratch,
        ),
        Err(external_error())
    );
    assert_eq!(scratch.retained_bytes(), retained);
    Ok(())
}

#[test]
fn staged_legacy_output_stays_unchanged_and_mutable_strings_remain_checked() -> Result<()> {
    let owned = build::parse(
        Payload::from_slice(br#"{"a":"\u00e9"}"#),
        Limits::strict_default(),
    )?;
    let mut legacy = owned.to_legacy()?;
    let mut output = b"prefix".to_vec();
    let limits = EncodeLimits {
        max_output_bytes: 1,
        ..EncodeLimits::default()
    };
    assert!(typed::encode_document(&legacy, &mut output, limits).is_err());
    assert_eq!(output, b"prefix");
    if let Some(reference) = legacy.parts.node_metadata[1].string.as_mut() {
        reference.scalar_len += 1;
    }
    assert_eq!(
        typed::write_document(&legacy, &mut output, EncodeLimits::default())
            .err()
            .map(|error| error.code),
        Some(ErrorCode::ConstraintViolation)
    );
    assert_eq!(output, b"prefix");
    Ok(())
}

#[test]
fn limited_sink_reuses_reservations_but_checks_every_write_and_overflow() -> Result<()> {
    let mut output = RecordingSink::default();
    let mut bounded = LimitedSink::new(&mut output, 3);
    bounded.reserve(3)?;
    bounded.write_bytes(b"a")?;
    bounded.reserve(2)?;
    bounded.write_bytes(b"bc")?;
    assert_eq!(
        bounded.reserve(1).err().map(|error| error.code),
        Some(ErrorCode::PayloadTooLarge)
    );
    assert_eq!(
        bounded.write_bytes(b"d").err().map(|error| error.code),
        Some(ErrorCode::PayloadTooLarge)
    );
    assert_eq!(output.reservations, [(0, 3)]);
    assert_eq!(output.bytes, b"abc");
    let mut output = RecordingSink::default();
    let mut bounded = LimitedSink::new(&mut output, usize::MAX);
    bounded.write_byte(b'a')?;
    assert_eq!(
        bounded.reserve(usize::MAX).err().map(|error| error.code),
        Some(ErrorCode::PayloadTooLarge)
    );
    Ok(())
}

struct BrokenSink {
    length: usize,
    mutate_reserve: bool,
}

impl ByteSink for BrokenSink {
    fn reserve(&mut self, _additional: usize) -> Result<()> {
        if self.mutate_reserve {
            self.length += 1;
        }
        Ok(())
    }
    fn write_bytes(&mut self, _bytes: &[u8]) -> Result<()> {
        // Incorrectly reports success without appending the requested bytes.
        Ok(())
    }
    fn len(&self) -> usize {
        self.length
    }
}

#[test]
fn malformed_external_sink_lengths_poison_and_prefix_overflow_is_rejected() -> Result<()> {
    for mutate_reserve in [true, false] {
        let mut output = BrokenSink {
            length: 0,
            mutate_reserve,
        };
        let mut bounded = LimitedSink::new(&mut output, 16);
        let result = if mutate_reserve {
            bounded.reserve(4)
        } else {
            bounded.write_byte(b'a')
        };
        assert_eq!(
            result.err().map(|error| error.code),
            Some(ErrorCode::InternalError)
        );
        assert!(bounded.is_poisoned());
        assert_eq!(
            bounded.reserve(1).err().map(|error| error.code),
            Some(ErrorCode::InternalError)
        );
    }
    let mut output = BrokenSink {
        length: usize::MAX - 1,
        mutate_reserve: false,
    };
    let mut writer = JsonWriter::new(&mut output, EncodeLimits::default());
    assert_eq!(
        writer.string("").err().map(|error| error.code),
        Some(ErrorCode::PayloadTooLarge)
    );
    assert!(writer.finish().is_err());
    assert_eq!(output.length, usize::MAX - 1);
    Ok(())
}
