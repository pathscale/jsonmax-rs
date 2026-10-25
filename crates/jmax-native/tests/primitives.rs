//! Each Reader primitive against the authority it must agree with: the schema
//! VM for acceptance, and jmax-number, jmax-decimal and jmax-syntax for values.

use jmax_abi::number::{ConvertedNumber, DecimalPolicy, NumericPolicy};
use jmax_abi::syntax::DuplicatePolicyOwner;
use jmax_native::{NativeContext, Reader, VmLimits};
use jmax_schema_aot::compile_graph;
use jmax_schema_ir::{
    DecimalConstraints, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode, ValueKind,
};
use jmax_schema_vm::{ValidatedProgram, ValidationScratch};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn scalar_program(constraints: ScalarConstraints) -> ValidatedProgram {
    compile_graph(&Schema {
        name: "root".into(),
        schema_id: SchemaId(1),
        root: TypeId(0),
        nodes: vec![TypeNode::Scalar(constraints)],
        definitions: vec![],
    })
    .unwrap()
}

fn vm_accepts(program: &ValidatedProgram, input: &[u8], limits: VmLimits, numeric: NumericPolicy) -> bool {
    jmax_schema_vm::direct::validate_borrowed_into(
        program, input, limits, numeric, &mut ValidationScratch::default(),
    )
    .is_ok()
}

fn numeric_variants() -> Vec<NumericPolicy> {
    let strict = NumericPolicy::strict_default();
    vec![
        strict,
        NumericPolicy { allow_Quoted_int: true, ..strict },
        NumericPolicy { reject_negative_zero: false, ..strict },
        NumericPolicy { allow_Quoted_int: true, reject_negative_zero: false, reject_non_finite_float: false, max_exponent_abs: 5 },
    ]
}

fn limit_variants() -> Vec<VmLimits> {
    let base = VmLimits::default();
    let mut digits = base;
    digits.syntax.max_number_digits = 3;
    let mut strings = base;
    strings.syntax.max_string_bytes = 6;
    vec![base, digits, strings]
}

fn context(program: &ValidatedProgram, limits: VmLimits, numeric: NumericPolicy) -> NativeContext {
    NativeContext::with_program(program, limits, numeric).unwrap()
}

fn token_corpus(rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut tokens: Vec<Vec<u8>> = [
        "0", "-0", "01", "-01", "1", "-1", "12", "123", "1234", "9", "10", "99",
        "9223372036854775807", "9223372036854775808", "-9223372036854775808", "-9223372036854775809",
        "18446744073709551615", "18446744073709551616", "1234567890123456789", "12345678901234567890",
        "1.0", "1.5", "-1.5", "0.0", "-0.0", "1e2", "1E2", "1e-2", "1e+2", "1e", "1e999", "1e308", "1e309",
        "-1e309", "1e-400", "-1e-400", "5e-324", "1.7976931348623157e308", "0.1", "00.1", ".5", "5.", "+1",
        "1_0", "0x10", "NaN", "Infinity", "-", "--1", "", "1 ", " 1", "\n1\t", "1,", "1]", "1}", "1:", "1x",
        "\"5\"", "\"-0\"", "\"12\"", "\"1.5\"", "\"\"", "\"\\u0031\"", "\"01\"", "\"1 \"", "\" 1\"", "\"1e2\"",
        "true", "false", "null", "tru", "truee", "true,", "false}", "True", "\"x\"", "[1]", "{}",
        "\"0.00100000\"", "\"100.00000000\"", "\"1\"", "\"-1.5\"", "\"1e2\"", "\"1E-2\"", "\"1.\"",
        "\"0.123456789\"", "\"123456789012345678901234567890\"", "\"99999999999999999999999999999999999999\"",
        "\"-0.00000000\"", "\"-0\"", "\".5\"", "\"+1\"", "\"1\\u002e5\"", "\"00\"", "\"0.5e1\"",
        "\"abc\"", "\"a\\nb\"", "\"\\ud83e\\udd80\"", "\"\\ud83e\"", "\"\\udd80\"", "\"\\q\"", "\"\\u12\"",
        "\"unterminated", "\"tab\there\"", "\"long string over six bytes\"", "\"é\"", "\"\u{1f980}\"",
    ]
    .iter()
    .map(|token| token.as_bytes().to_vec())
    .collect();
    tokens.push(b"\"\xff\"".to_vec());
    tokens.push(b"\"\xc3\"".to_vec());
    tokens.push(b"\"\x01\"".to_vec());
    let alphabet = b"0123456789-+.eE\" \tabu\\ntrfsl,]}";
    for _ in 0..3000 {
        let len = 1 + rng.below(12);
        tokens.push((0..len).map(|_| alphabet[rng.below(alphabet.len())]).collect());
    }
    for _ in 0..800 {
        let digits = 1 + rng.below(24);
        let mut token: Vec<u8> = (0..digits).map(|_| b'0' + rng.below(10) as u8).collect();
        if rng.below(3) == 0 {
            token.insert(0, b'-');
        }
        if rng.below(4) == 0 {
            token.insert(1.min(token.len()), b'.');
        }
        tokens.push(token);
    }
    tokens
}

/// The integer a native read produced, from whichever primitive fits the bounds.
fn native_integer(
    token: &[u8], ctx: &mut NativeContext, min: Option<i64>, max: Option<i64>,
) -> Option<i128> {
    let mut read = |f: &dyn Fn(&mut Reader, &mut NativeContext) -> Option<i128>| {
        let mut reader = Reader::new(token);
        f(&mut reader, ctx).filter(|_| reader.end().is_ok())
    };
    match (min, max) {
        (None, None) => read(&|r, c| r.uint(c, u64::MAX).ok().map(i128::from))
            .or_else(|| read(&|r, c| r.int(c, i64::MIN, i64::MAX).ok().map(i128::from))),
        (Some(lo), Some(hi)) if lo < 0 => read(&|r, c| r.int(c, lo, hi).ok().map(i128::from)),
        (Some(_), Some(hi)) => read(&|r, c| r.uint(c, hi as u64).ok().map(i128::from)),
        _ => unreachable!(),
    }
}

/// The text a quoted token decodes to, or the token itself, trimmed.
fn unquoted(token: &[u8], limits: VmLimits) -> Vec<u8> {
    let trimmed = token.trim_ascii();
    if trimmed.first() != Some(&b'"') {
        return trimmed.to_vec();
    }
    let mut decoded = Vec::new();
    let mut cursor = jmax_syntax::BlockCursor::new(trimmed, limits.syntax, None, DuplicatePolicyOwner::Syntax).unwrap();
    let meta = cursor.scan_string_meta(&mut decoded).unwrap();
    match meta.cooked.source {
        jmax_abi::span::SpanSource::RawPayload => meta.cooked.span.slice(trimmed).unwrap().to_vec(),
        _ => meta.cooked.span.slice(&decoded).unwrap().to_vec(),
    }
}

#[test]
fn integers_match_the_vm_and_jmax_number() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let tokens = token_corpus(&mut rng);
    let bounds: [(Option<i64>, Option<i64>); 5] = [
        (None, None),
        (Some(i64::MIN), Some(i64::MAX)),
        (Some(0), Some(i64::MAX)),
        (Some(0), Some(i64::from(u32::MAX))),
        (Some(-5), Some(100)),
    ];
    let (mut accepted, mut compared) = (0u32, 0u32);
    for (min, max) in bounds {
        let mut constraints = ScalarConstraints::new(ValueKind::Integer);
        constraints.min_i64 = min;
        constraints.max_i64 = max;
        let program = scalar_program(constraints);
        for numeric in numeric_variants() {
            for limits in limit_variants() {
                let mut ctx = context(&program, limits, numeric);
                for token in &tokens {
                    let vm = vm_accepts(&program, token, limits, numeric);
                    let native = native_integer(token, &mut ctx, min, max);
                    compared += 1;
                    match native {
                        Some(value) => {
                            assert!(vm, "native accepted {:?} that the VM rejects ({min:?}..{max:?}, {numeric:?})", String::from_utf8_lossy(token));
                            accepted += 1;
                            let text = unquoted(token, limits);
                            let lexed = jmax_number::lex::lex_number(&text, 0, &ctx.numeric()).unwrap();
                            let expected = match jmax_number::integer::convert_integer(&lexed).unwrap() {
                                ConvertedNumber::U64(v) => i128::from(v),
                                ConvertedNumber::I64(v) => i128::from(v),
                                _ => unreachable!(),
                            };
                            assert_eq!(value, expected, "{:?}", String::from_utf8_lossy(token));
                        }
                        None => assert!(
                            !vm,
                            "native rejected {:?} that the VM accepts ({min:?}..{max:?}, {numeric:?}, digits {})",
                            String::from_utf8_lossy(token), limits.syntax.max_number_digits
                        ),
                    }
                }
            }
        }
    }
    assert!(compared > 100_000 && accepted > 5_000, "{compared} {accepted}");
}

#[test]
fn floats_match_the_vm_and_jmax_number() {
    let mut rng = Rng(0x0bad_cafe_f00d_1111);
    let tokens = token_corpus(&mut rng);
    let program = scalar_program(ScalarConstraints::new(ValueKind::Number));
    let (mut accepted, mut compared) = (0u32, 0u32);
    for numeric in numeric_variants() {
        for limits in limit_variants() {
            let mut ctx = context(&program, limits, numeric);
            for token in &tokens {
                let vm = vm_accepts(&program, token, limits, numeric);
                let mut reader = Reader::new(token);
                let native = reader.float(&mut ctx).ok().filter(|_| reader.end().is_ok());
                compared += 1;
                match native {
                    Some(value) => {
                        assert!(vm, "native accepted {:?} that the VM rejects ({numeric:?})", String::from_utf8_lossy(token));
                        accepted += 1;
                        let text = core::str::from_utf8(token.trim_ascii()).unwrap();
                        assert_eq!(value.to_bits(), text.parse::<f64>().unwrap().to_bits(), "{text}");
                        let lexed = jmax_number::lex::lex_number(text.as_bytes(), 0, &ctx.numeric()).unwrap();
                        let reference = jmax_number::float::parse_f64(&lexed, &ctx.numeric()).unwrap();
                        assert_eq!(value.to_bits(), reference.to_bits());
                    }
                    None => assert!(
                        !vm,
                        "native rejected {:?} that the VM accepts ({numeric:?}, digits {})",
                        String::from_utf8_lossy(token), limits.syntax.max_number_digits
                    ),
                }
            }
        }
    }
    assert!(compared > 10_000 && accepted > 1_000, "{compared} {accepted}");
}

fn decimal_program(policy: DecimalPolicy) -> ValidatedProgram {
    let mut constraints = ScalarConstraints::new(ValueKind::Decimal);
    constraints.decimal = Some(DecimalConstraints { policy, minimum: None, maximum: None });
    scalar_program(constraints)
}

#[test]
fn decimals_match_the_vm_and_jmax_decimal() {
    let mut rng = Rng(0xdec1_ba11_0000_0001);
    let mut tokens = token_corpus(&mut rng);
    for _ in 0..1500 {
        let pow = rng.below(12) as u32;
        let int = rng.below(10u64.pow(pow) as usize);
        let frac_len = rng.below(10);
        let frac: String = (0..frac_len).map(|_| char::from(b'0' + rng.below(10) as u8)).collect();
        let sign = ["", "-"][rng.below(2)];
        let exp = ["", "e2", "E-3", "e+1"][rng.below(4)];
        let dot = if frac_len == 0 { "" } else { "." };
        tokens.push(format!("\"{sign}{int}{dot}{frac}{exp}\"").into_bytes());
    }
    let spot = DecimalPolicy::spot_8();
    let policies = [
        spot,
        DecimalPolicy { allow_exponent: true, ..spot },
        DecimalPolicy { max_precision: 5, target_scale: 2, allow_exponent: true, reject_inexact_rescale: false, tick_step: 5 },
        DecimalPolicy { max_precision: 38, target_scale: 0, allow_exponent: false, reject_inexact_rescale: true, tick_step: 0 },
    ];
    let (mut accepted, mut compared) = (0u32, 0u32);
    for policy in policies {
        let program = decimal_program(policy);
        for numeric in numeric_variants() {
            for limits in limit_variants() {
                let mut ctx = context(&program, limits, numeric);
                for token in &tokens {
                    let vm = vm_accepts(&program, token, limits, numeric);
                    let mut reader = Reader::new(token);
                    let native = reader.decimal(&mut ctx, &policy).ok().filter(|_| reader.end().is_ok());
                    compared += 1;
                    match native {
                        Some(value) => {
                            assert!(vm, "native accepted {:?} that the VM rejects ({policy:?})", String::from_utf8_lossy(token));
                            accepted += 1;
                            // jmax-decimal parses escape-free quoted tokens on its own.
                            if !token.contains(&b'\\') {
                                let trimmed = token.trim_ascii();
                                let reference = jmax_decimal::parse::parse_quoted(trimmed, 0, &policy).unwrap();
                                assert_eq!(value, reference, "{:?}", String::from_utf8_lossy(token));
                            }
                        }
                        None => assert!(
                            !vm,
                            "native rejected {:?} that the VM accepts ({policy:?}, {numeric:?}, digits {})",
                            String::from_utf8_lossy(token), limits.syntax.max_number_digits
                        ),
                    }
                }
            }
        }
    }
    assert!(compared > 50_000 && accepted > 3_000, "{compared} {accepted}");
}

#[test]
fn strings_match_the_vm_and_jmax_syntax() {
    let mut rng = Rng(0x57a1_1ce5_0000_0003);
    let mut tokens = token_corpus(&mut rng);
    let alphabet: &[&str] = &["a", "é", "\\n", "\\u0041", "\\ud83e\\udd80", "\\\"", "\\\\", "\\/", "x", "\u{1f980}", "\\ud800", "\\q", "\u{1}"];
    for _ in 0..2000 {
        let mut text = String::from("\"");
        for _ in 0..rng.below(8) {
            text.push_str(alphabet[rng.below(alphabet.len())]);
        }
        if rng.below(10) != 0 {
            text.push('"');
        }
        tokens.push(text.into_bytes());
    }
    // Around the 32 byte reach of the inline scan: a special byte at each position
    // from 29 to 34, and plain runs of 28 to 36 bytes, closed or not.
    for length in 28..=36usize {
        for special in ["", "\\n", "é", "\u{1}", "\\u0041", "\\\"", "\u{1f980}"] {
            for at in 29..=34usize {
                if at > length {
                    continue;
                }
                let mut text = String::from("\"");
                text.push_str(&"a".repeat(at));
                text.push_str(special);
                text.push_str(&"b".repeat(length - at));
                tokens.push(text.clone().into_bytes());
                text.push('"');
                tokens.push(text.into_bytes());
            }
        }
    }
    // Bytes that are not UTF-8 (a lone continuation, 0xff) near the boundary.
    for byte in [0x80u8, 0xbf, 0xc3, 0xff] {
        for at in [0usize, 5, 30, 31, 32, 33] {
            let mut token = std::vec![b'"'];
            token.extend(std::iter::repeat(b'a').take(at));
            token.push(byte);
            token.extend_from_slice(b"zz\"");
            tokens.push(token);
        }
    }
    let program = scalar_program(ScalarConstraints::new(ValueKind::String));
    let numeric = NumericPolicy::strict_default();
    let (mut accepted, mut compared) = (0u32, 0u32);
    for limits in limit_variants() {
        let mut ctx = context(&program, limits, numeric);
        for token in &tokens {
            let vm = vm_accepts(&program, token, limits, numeric);
            let mut reader = Reader::new(token);
            let native = reader.string(&mut ctx).ok().map(str::to_owned).filter(|_| reader.end().is_ok());
            // key_bytes has the same accept set and the same bytes as string.
            let keyed = {
                let mut with_colon = token.clone();
                with_colon.push(b':');
                let mut reader = Reader::new(&with_colon);
                reader.key_bytes(&mut ctx).ok().map(<[u8]>::to_vec).filter(|_| reader.end().is_ok())
            };
            assert_eq!(keyed, native.as_ref().map(|text| text.as_bytes().to_vec()), "{:?}", String::from_utf8_lossy(token));
            compared += 1;
            match native {
                Some(text) => {
                    assert!(vm, "native accepted {:?} that the VM rejects", String::from_utf8_lossy(token));
                    accepted += 1;
                    let mut decoded = Vec::new();
                    let mut cursor = jmax_syntax::BlockCursor::new(token, limits.syntax, None, DuplicatePolicyOwner::Syntax).unwrap();
                    cursor.skip_whitespace().unwrap();
                    let meta = cursor.scan_string_meta(&mut decoded).unwrap();
                    let bytes = match meta.cooked.source {
                        jmax_abi::span::SpanSource::RawPayload => meta.cooked.span.slice(token),
                        _ => meta.cooked.span.slice(&decoded),
                    }
                    .unwrap();
                    assert_eq!(text.as_bytes(), bytes);
                }
                None => assert!(
                    !vm,
                    "native rejected {:?} that the VM accepts (max_string_bytes {})",
                    String::from_utf8_lossy(token), limits.syntax.max_string_bytes
                ),
            }
        }
    }
    assert!(compared > 10_000 && accepted > 1_000, "{compared} {accepted}");
}

#[test]
fn booleans_match_the_vm() {
    let mut rng = Rng(0xb001_b001_b001_b001);
    let tokens = token_corpus(&mut rng);
    let program = scalar_program(ScalarConstraints::new(ValueKind::Boolean));
    let numeric = NumericPolicy::strict_default();
    let limits = VmLimits::default();
    let mut ctx = context(&program, limits, numeric);
    let mut accepted = 0;
    for token in &tokens {
        let vm = vm_accepts(&program, token, limits, numeric);
        let mut reader = Reader::new(token);
        let native = reader.boolean().ok().filter(|_| reader.end().is_ok());
        assert_eq!(native.is_some(), vm, "{:?}", String::from_utf8_lossy(token));
        accepted += usize::from(vm);
    }
    assert!(accepted >= 2);
    let _ = &mut ctx;
}

#[test]
fn skip_value_matches_the_cursor() {
    let mut rng = Rng(0x5c1b_0000_0000_0001u64);
    let seeds = [
        r#"{"a":1,"b":[true,false,null],"c":{"d":"e\né🦀","f":-0.5e10}}"#,
        r#"[{"k":"v","k2":"\"\\\/\b\f\n\r\t"},{"k":"v2"},[],{},[[[[{}]]]]]"#,
        r#"{"dup":1,"dup":2}"#, r#"{"a":1,"a":2}"#, r#"[1,2,"#, "tru", "-0", "[1e999]", "\"x\"",
    ];
    let program = scalar_program(ScalarConstraints::new(ValueKind::Number));
    let (mut accepted, mut compared) = (0, 0);
    for seed in seeds {
        for round in 0..400 {
            let mut input = seed.as_bytes().to_vec();
            for _ in 0..(round % 3) {
                if input.is_empty() { break; }
                let at = rng.below(input.len());
                match rng.below(3) {
                    0 => input[at] = b"{}[]:,\"\\ 0-e1tn"[rng.below(15)],
                    1 => { input.remove(at); }
                    _ => input.truncate(at),
                }
            }
            for limits in limit_variants() {
                for numeric in numeric_variants() {
                    let mut ctx = context(&program, limits, numeric);
                    let mut reader = Reader::new(&input);
                    let depth = (round % 4) as u16;
                    let native = reader.skip_value(&mut ctx, depth).ok().map(|()| reader.pos());
                    let owner = if limits.syntax.reject_duplicate_keys { DuplicatePolicyOwner::SchemaVm } else { DuplicatePolicyOwner::Disabled };
                    let mut decoded = Vec::new();
                    let reference = jmax_syntax::BlockCursor::new(&input, limits.syntax, Some(ctx.numeric()), owner)
                        .and_then(|mut c| {
                            c.skip_value(&mut decoded, depth, jmax_abi::syntax::SkipMode::StrictValidate)?;
                            Ok(c.offset() as usize)
                        })
                        .ok();
                    assert_eq!(native, reference, "{:?}", String::from_utf8_lossy(&input));
                    compared += 1;
                    accepted += usize::from(native.is_some());
                }
            }
        }
    }
    assert!(compared > 10_000 && accepted > 1_000, "{compared} {accepted}");
}

#[test]
fn unknown_keys_detect_duplicates_per_object() {
    let program = scalar_program(ScalarConstraints::new(ValueKind::Number));
    let numeric = NumericPolicy::strict_default();
    let mut ctx = context(&program, VmLimits::default(), numeric);
    let outer = ctx.begin_object();
    ctx.unknown_key("a").unwrap();
    ctx.unknown_key("b").unwrap();
    // A nested object may reuse its parent's keys.
    let inner = ctx.begin_object();
    ctx.unknown_key("a").unwrap();
    ctx.unknown_key("c").unwrap();
    ctx.end_object(inner).unwrap();
    ctx.unknown_key("c").unwrap();
    ctx.end_object(outer).unwrap();
    // Duplicates are found whatever their order, and the scope is always cleared.
    let object = ctx.begin_object();
    for key in ["x", "y", "z", "q", "y"] {
        ctx.unknown_key(key).unwrap();
    }
    assert!(ctx.end_object(object).is_err());
    let object = ctx.begin_object();
    ctx.unknown_key("y").unwrap();
    ctx.end_object(object).unwrap();
    // With duplicate rejection off nothing is reported.
    let mut limits = VmLimits::default();
    limits.syntax.reject_duplicate_keys = false;
    let mut ctx = context(&program, limits, numeric);
    let object = ctx.begin_object();
    ctx.unknown_key("a").unwrap();
    ctx.unknown_key("a").unwrap();
    ctx.end_object(object).unwrap();
}

#[test]
fn reader_unknown_key_records_decoded_keys() {
    let program = scalar_program(ScalarConstraints::new(ValueKind::Number));
    let numeric = NumericPolicy::strict_default();
    let mut ctx = context(&program, VmLimits::default(), numeric);
    // An escaped spelling and a plain one are the same decoded key.
    let escaped_a: &[u8] = b" \"\\u0061\" : ";
    let plain_a: &[u8] = b"\"a\":";
    let plain_b: &[u8] = b"\"b\":";
    let escaped_pair: &[u8] = b"\"\\ud83d\\ude00\":";
    let plain_pair: &[u8] = "\"\u{1f600}\":".as_bytes();
    let object = ctx.begin_object();
    for text in [escaped_a, plain_a, plain_b] {
        let mut reader = Reader::new(text);
        reader.unknown_key(&mut ctx).unwrap();
        assert_eq!(reader.pos(), text.len() - usize::from(text.ends_with(b" ")));
    }
    assert!(ctx.end_object(object).is_err());
    let object = ctx.begin_object();
    for text in [plain_a, plain_b, escaped_pair] {
        Reader::new(text).unknown_key(&mut ctx).unwrap();
    }
    assert!(ctx.end_object(object).is_ok());
    let object = ctx.begin_object();
    for text in [escaped_pair, plain_pair] {
        Reader::new(text).unknown_key(&mut ctx).unwrap();
    }
    assert!(ctx.end_object(object).is_err());
    // A missing colon, an unterminated or invalid string is a Fail.
    let bad: [&[u8]; 5] = [b"\"a\"", b"\"a", b"a:", b"\"\\ud800\":", b"\"\x01\":"];
    for text in bad {
        assert!(Reader::new(text).unknown_key(&mut ctx).is_err(), "{text:?}");
    }
}

#[test]
fn match_key_hits_only_exact_unescaped_keys() {
    let program = scalar_program(ScalarConstraints::new(ValueKind::Number));
    let ctx = context(&program, VmLimits::default(), NumericPolicy::strict_default());
    let hit = |input: &[u8], pattern: &[u8]| {
        let mut reader = Reader::new(input);
        let matched = reader.match_key(&ctx, pattern);
        (matched, reader.pos())
    };
    assert_eq!(hit(b"\"a\":1", b"\"a\":"), (true, 4));
    assert_eq!(hit(b" \n\"a\":1", b"\"a\":"), (true, 6));
    // A miss consumes only whitespace.
    assert_eq!(hit(b" \"b\":1", b"\"a\":"), (false, 1));
    assert_eq!(hit(b"\"ab\":1", b"\"a\":"), (false, 0));
    assert_eq!(hit(b"\"a\" :1", b"\"a\":"), (false, 0));
    assert_eq!(hit(b"\"\\u0061\":1", b"\"a\":"), (false, 0));
    assert_eq!(hit(b"\"a\"", b"\"a\":"), (false, 0));
    assert_eq!(hit(b"", b"\"a\":"), (false, 0));
    // Patterns that are not `"name":` never match.
    for pattern in [&b""[..], b"a:", b"\"a\"", b"\"a:", b"\":"] {
        assert!(!hit(b"\"a\":1", pattern).0, "{pattern:?}");
    }
    assert_eq!(hit(b"\"\":1", b"\"\":"), (true, 3));
    // A name longer than max_string_bytes is left to `key`, which rejects it.
    let mut limits = VmLimits::default();
    limits.syntax.max_string_bytes = 3;
    let small = context(&program, limits, NumericPolicy::strict_default());
    let mut reader = Reader::new(b"\"abc\":1");
    assert!(reader.match_key(&small, b"\"abc\":"));
    let mut reader = Reader::new(b"\"abcd\":1");
    assert!(!reader.match_key(&small, b"\"abcd\":"));
}

/// Strings around every format's grammar: valid, nearly valid, escaped and noisy.
fn format_tokens(rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut tokens = token_corpus(rng);
    let samples = [
        "123e4567-e89b-12d3-a456-426614174000", "123E4567-E89B-12D3-A456-426614174000",
        "123e4567e89b12d3a456426614174000", "00000000-0000-0000-0000-000000000000",
        "192.168.0.1", "192.168.00.1", "256.1.1.1", "::1", "::", "2001:db8::ff00:42:8329", "1::2::3",
        "::ffff:192.0.2.1", "fe80::1%eth0", "1.2.3", "",
        "AQID", "AQI=", "AQ==", "AQ", "AQJ=", "AR==", "////", "++++", "-_-_", "Zm9vYg==", "Zm9vYmE=", "Zm9vYmFy",
        "0x00112233445566778899aabbccddeeff00112233", "0X00112233445566778899aabbccddeeff00112233",
        "0x00112233445566778899AABBCCDDEEFF0011223", "0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        "0x00112233445566778899aabbccddeeff00112233445566778899aabbccddeef",
        "V1StGXR8_Z5jdHi6B-myT", "V1StGXR8_Z5jdHi6B-my", "V1StGXR8_Z5jdHi6B-myTT", "V1StGXR8 Z5jdHi6B-myT",
        "abcdefghijklmnop", "abcdefghijklmno", "abcdefghijklmnopq", "abcdefghij_lmnop", "abcdefghij-lmnop",
        "abcdefghij\u{e9}lmnop", "abcdefghij\u{e9}lmno", "a", "AZaz09", "a_", "a-",
    ];
    for sample in samples {
        tokens.push(format!("\"{sample}\"").into_bytes());
        // The same text with its first and last characters escaped.
        if sample.len() > 2 && sample.is_ascii() {
            let (head, rest) = sample.split_at(1);
            tokens.push(format!("\"\\u{:04x}{rest}\"", head.as_bytes()[0]).into_bytes());
        }
    }
    let alphabet = b"0123456789abcdefABCDEF-_:.=+/xg ";
    for _ in 0..1500 {
        let length = rng.below(48);
        let mut token = vec![b'"'];
        token.extend((0..length).map(|_| alphabet[rng.below(alphabet.len())]));
        token.push(b'"');
        tokens.push(token);
    }
    tokens
}

#[test]
fn formats_match_the_vm() {
    use jmax_native::FormatId;
    let mut rng = Rng(0x0f0a_7a11_c0de_f001);
    let tokens = format_tokens(&mut rng);
    let numeric = NumericPolicy::strict_default();
    let formats = [
        ("uuid", FormatId::Uuid), ("ipv4", FormatId::Ipv4), ("ipv6", FormatId::Ipv6), ("ip", FormatId::IpAddr),
        ("base64", FormatId::Base64), ("base64url", FormatId::Base64Url), ("hex", FormatId::Hex),
        ("nanoid", FormatId::NanoId), ("blockchain-address", FormatId::BlockchainAddress),
        ("blockchain-transaction-hash", FormatId::BlockchainTransactionHash),
        ("nanoid-chars", FormatId::NanoIdChars), ("base62", FormatId::Base62),
    ];
    let (mut accepted, mut compared) = (0u32, 0u32);
    for (name, id) in formats {
        // With no length bound, and with the exact lengths 16 and 21.
        for exact in [None, Some(16u32), Some(21)] {
            let mut constraints = ScalarConstraints::new(ValueKind::String);
            constraints.format = Some(name.into());
            constraints.min_length = exact;
            constraints.max_length = exact;
            let program = scalar_program(constraints);
            for limits in limit_variants() {
                let mut ctx = context(&program, limits, numeric);
                for token in &tokens {
                    let vm = vm_accepts(&program, token, limits, numeric);
                    let mut reader = Reader::new(token);
                    let native = reader.format(&mut ctx, id, exact).ok().map(str::to_owned).filter(|_| reader.end().is_ok());
                    compared += 1;
                    assert_eq!(
                        vm,
                        native.is_some(),
                        "{name} {exact:?} {limits:?}: {:?}",
                        String::from_utf8_lossy(token)
                    );
                    accepted += u32::from(native.is_some());
                }
            }
        }
    }
    assert!(compared > 100_000 && accepted > 500, "{compared} {accepted}");
}

#[test]
fn bytea_decodes_what_the_base64_format_accepts() {
    let mut constraints = ScalarConstraints::new(ValueKind::String);
    constraints.format = Some("base64".into());
    let program = scalar_program(constraints);
    let numeric = NumericPolicy::strict_default();
    let mut ctx = context(&program, VmLimits::default(), numeric);
    let mut out = vec![9u8];
    for (text, bytes) in [
        ("", &b""[..]), ("Zg==", b"f"), ("Zm8=", b"fo"), ("Zm9v", b"foo"), ("Zm9vYg==", b"foob"),
        ("Zm9vYmE=", b"fooba"), ("Zm9vYmFy", b"foobar"), ("AAECAwQF", &[0, 1, 2, 3, 4, 5]), ("/+8=", &[0xff, 0xef]),
    ] {
        let token = format!("\"{text}\"");
        let mut reader = Reader::new(token.as_bytes());
        reader.bytea(&mut ctx, &mut out).unwrap();
        assert_eq!(out, bytes, "{text}");
        assert!(reader.end().is_ok());
    }
    // Exhaustive over every one, two and three byte value pair: encode, decode, compare.
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for a in 0..=255u8 {
        for b in [0u8, 1, 127, 128, 255, a] {
            for c in [0u8, 77, 255] {
                for length in 1..=3usize {
                    let raw = [a, b, c];
                    let raw = &raw[..length];
                    let mut text = String::from("\"");
                    for chunk in raw.chunks(3) {
                        let n = chunk.iter().enumerate().fold(0u32, |n, (i, v)| n | u32::from(*v) << (16 - 8 * i));
                        for k in 0..chunk.len() + 1 {
                            text.push(table[(n >> (18 - 6 * k) & 63) as usize] as char);
                        }
                        for _ in chunk.len() + 1..4 {
                            text.push('=');
                        }
                    }
                    text.push('"');
                    let mut reader = Reader::new(text.as_bytes());
                    reader.bytea(&mut ctx, &mut out).unwrap();
                    assert_eq!(out, raw, "{text}");
                }
            }
        }
    }
    // Rejected spellings leave no claim about `out` but Fail.
    for bad in ["\"Zg=\"", "\"Zg\"", "\"Zh==\"", "\"Zm9\"", "\"Zm9v=\"", "\"Z===\"", "\"Zm 9\"", "\"é\""] {
        assert!(Reader::new(bad.as_bytes()).bytea(&mut ctx, &mut out).is_err(), "{bad}");
    }
}
