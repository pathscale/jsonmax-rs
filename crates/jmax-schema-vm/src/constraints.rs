//! Scalar conversion and inclusive schema constraints.
//!
//! Runtime byte/digit limits belong to `execute`, which has the `VmLimits`.
//! These entry points independently bound their work with the same VM budget
//! and never issue a proof or publish a partially checked slot.
use super::*;
use jmax_abi::number::NumericPolicy;
use jmax_schema_ir::DecimalConstraints;

/// Validate one complete scalar token and return its natural ABI representation.
///
/// Decoded string references are checked against both their owner and the raw
/// token. All diagnostics from decoded text identify the enclosing raw token.
pub fn check_scalar(
    program: &ValidatedProgram,
    state: Option<StateId>,
    value: ScalarInput<'_>,
    budget: &mut VmBudget,
) -> Result<SlotValue> {
    let offset = value.span.offset;
    budget.charge(1, offset)?;
    let current = match state {
        Some(id) => {
            let index = usize::try_from(id.0).map_err(|_| internal(offset))?;
            let current = program.program().states.get(index).ok_or_else(|| internal(offset))?;
            if current.id != id { return Err(internal(offset)); }
            Some(current)
        }
        None => None,
    };
    let kind = match current.map(|current| &current.kind) {
        None | Some(StateKind::Any) => None,
        Some(StateKind::Scalar(kind)) => Some(*kind),
        Some(StateKind::Object { .. } | StateKind::Array { .. }) => return Err(type_error(offset)),
    };
    let token = value.span.slice(value.raw).ok_or_else(|| internal(offset))?;
    if token.is_empty() {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset, "empty scalar token"));
    }
    let numeric = &program.program().policy.numeric;
    let text = match (value.kind, value.decoded) {
        (ScalarKind::String, Some(reference)) => {
            // Prepay UTF-8 validation, scalar counting and token comparison.
            charge_bytes(budget, token.len(), 2, offset)?;
            let decoded_len = usize::try_from(reference.span.len).map_err(|_| internal(offset))?;
            charge_bytes(budget, decoded_len, 3, offset)?;
            let text = resolve_str(value.raw, value.arena, reference)
                .map_err(|error| at_token(error, offset))?;
            check_string_token(token, text, reference, value.span, offset)?;
            Some(text)
        }
        (ScalarKind::String, None) | (_, Some(_)) => return Err(internal(offset)),
        (ScalarKind::Null, None) if token == b"null" => None,
        (ScalarKind::True, None) if token == b"true" => None,
        (ScalarKind::False, None) if token == b"false" => None,
        (ScalarKind::Number, None) => None,
        _ => return Err(vm_error(ErrorCode::InvalidSyntax, offset,
            "scalar kind disagrees with its raw token")),
    };
    match (kind, value.kind) {
        (None | Some(ValueKind::Null), ScalarKind::Null) => Ok(SlotValue::RawSpan(value.span)),
        (None | Some(ValueKind::Boolean), ScalarKind::True) => Ok(SlotValue::Bool(true)),
        (None | Some(ValueKind::Boolean), ScalarKind::False) => Ok(SlotValue::Bool(false)),
        (None | Some(ValueKind::Integer | ValueKind::Number), ScalarKind::Number) => {
            let lexeme = number_grammar(token, numeric, budget, offset)?;
            match kind {
                Some(ValueKind::Integer) => {
                    if !lexeme.integral { return Err(type_error(offset)); }
                    let converted = integer(token, lexeme.negative, offset)?;
                    integer_bounds(converted, &current.ok_or_else(|| internal(offset))?.constraints, offset)?;
                    Ok(converted)
                }
                Some(ValueKind::Number) => float(token, numeric, offset),
                None => {
                    if lexeme.integral {
                        match integer(token, lexeme.negative, offset) {
                            Ok(value) => return Ok(value),
                            Err(error) if error.code == ErrorCode::NumberOverflow => {}
                            Err(error) => return Err(error),
                        }
                    }
                    float(token, numeric, offset)
                }
                _ => Err(internal(offset)),
            }
        }
        (Some(ValueKind::Integer), ScalarKind::String) if numeric.allow_Quoted_int => {
            let text = text.ok_or_else(|| internal(offset))?;
            let lexeme = number_grammar(text.as_bytes(), numeric, budget, offset)?;
            if !lexeme.integral { return Err(type_error(offset)); }
            let converted = integer(text.as_bytes(), lexeme.negative, offset)?;
            integer_bounds(converted, &current.ok_or_else(|| internal(offset))?.constraints, offset)?;
            Ok(converted)
        }
        (Some(ValueKind::Decimal), ScalarKind::String) => {
            let constraints = current.and_then(|current| current.constraints.decimal.as_ref())
                .ok_or_else(|| internal(offset))?;
            decimal(text.ok_or_else(|| internal(offset))?, constraints, numeric, budget, offset)
                .map(SlotValue::Decimal)
        }
        (None | Some(ValueKind::String), ScalarKind::String) => {
            let reference = value.decoded.ok_or_else(|| internal(offset))?;
            if let Some(current) = current {
                check_count(&current.constraints, reference.scalar_len, offset)?;
                let text = text.ok_or_else(|| internal(offset))?;
                if let Some(id) = current.constraints.pattern {
                    let index = usize::try_from(id).map_err(|_| internal(offset))?;
                    let pattern = program.program().patterns.get(index).ok_or_else(|| internal(offset))?;
                    check_pattern(pattern, text, budget, offset)?;
                }
                if let Some(format) = current.constraints.format {
                    check_format(format, text, &program.program().format_options, budget, offset)?;
                }
            }
            Ok(SlotValue::Str(reference))
        }
        _ => Err(type_error(offset)),
    }
}

/// Check inclusive Unicode-scalar, array-item, or object-member bounds.
pub fn check_count(constraints: &ConstraintSet, count: u32, offset: u32) -> Result<()> {
    if constraints.min_count.is_some_and(|minimum| count < minimum)
        || constraints.max_count.is_some_and(|maximum| count > maximum)
    {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "count is outside inclusive schema bounds"));
    }
    Ok(())
}

/// Execute a Unicode pattern with the caller's remaining work allowance.
pub fn check_pattern(
    program: &PatternProgram,
    text: &str,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    // patterns::is_match holds one u64 visitation vector and three u32
    // worklists, each bounded by the instruction count. Its earlier validation
    // uses a bool vector and a u32 worklist, dropped before matching. Reserve
    // conservatively for those arrays and their minimum allocation capacities;
    // text is streamed and requires no input-sized scratch allocation.
    let states = u64::try_from(program.ops.len()).map_err(|_| resource(offset))?;
    let bytes = u64::try_from(text.len()).map_err(|_| resource(offset))?;
    let scratch = states.checked_mul(128)
        .and_then(|size| size.checked_add(256))
        .and_then(|size| u32::try_from(size).ok()).ok_or_else(|| resource(offset))?;
    // Raw PatternPrograms are accepted here, so pay for structural validation
    // and class-range scanning as well as the matcher's own transition charges.
    let setup = states.checked_mul(32).and_then(|units| units.checked_add(bytes))
        .and_then(|units| units.checked_add(1)).ok_or_else(|| resource(offset))?;
    budget.charge(setup, offset)?;
    for op in &program.ops {
        if let jmax_schema_compile::PatternOp::Class { ranges, .. } = op {
            charge_bytes(budget, ranges.len(), 2, offset)?;
        }
    }
    budget.retain(scratch, offset)?;
    let result = jmax_schema_compile::patterns::is_match(program, text, &mut budget.steps_left);
    budget.release(scratch)?;
    match result.map_err(|error| at_token(error, offset))? {
        true => Ok(()),
        false => Err(vm_error(ErrorCode::PatternMismatch, offset,
            "string does not match the compiled pattern")),
    }
}

fn check_format(
    format: jmax_format::FormatId,
    text: &str,
    options: &jmax_format::FormatOptions,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    // These allocation-free validators are linear, with a fixed number of
    // passes. Charge before dispatch so a format cannot reset the VM budget.
    budget.charge(1, offset)?;
    if u64::try_from(text.len()).map_err(|_| resource(offset))? > u64::from(options.max_input_bytes) {
        return Err(jmax_format::FormatFailure::new(jmax_format::FormatFailureKind::Limit, 0)
            .into_error(offset, ErrorSource::SchemaVm));
    }
    charge_bytes(budget, text.len(), 8, offset)?;
    jmax_format::registry::validate(format, text.as_bytes(), options)
        .map_err(|failure| failure.into_error(offset, ErrorSource::SchemaVm))
}

fn integer_bounds(value: SlotValue, constraints: &ConstraintSet, offset: u32) -> Result<()> {
    // i128 represents every u64 and i64 exactly, including both endpoints.
    let value = match value {
        SlotValue::U64(value) => i128::from(value),
        SlotValue::I64(value) => i128::from(value),
        _ => return Err(internal(offset)),
    };
    if constraints.min_i64.is_some_and(|minimum| value < i128::from(minimum))
        || constraints.max_i64.is_some_and(|maximum| value > i128::from(maximum))
    {
        return Err(vm_error(ErrorCode::ConstraintViolation, offset,
            "integer is outside inclusive schema bounds"));
    }
    Ok(())
}

struct NumberGrammar<'a> {
    negative: bool,
    integral: bool,
    integer: &'a [u8],
    fraction: &'a [u8],
    exponent: i32,
    has_exponent: bool,
}

/// Full JSON number grammar, including policy checks before any conversion.
fn number_grammar<'a>(
    bytes: &'a [u8],
    policy: &NumericPolicy,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<NumberGrammar<'a>> {
    // Covers lexical validation, integer conversion and a floating fallback.
    charge_bytes(budget, bytes.len(), 4, offset)?;
    if policy.max_exponent_abs < 0 { return Err(internal(offset)); }
    let negative = bytes.first() == Some(&b'-');
    let mut cursor = usize::from(negative);
    let start = cursor;
    match bytes.get(cursor) {
        Some(b'0') => cursor += 1,
        Some(b'1'..=b'9') => {
            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) { cursor += 1; }
        }
        _ => return Err(invalid_number(offset)),
    }
    let integer = bytes.get(start..cursor).ok_or_else(|| internal(offset))?;
    let mut integral = true;
    let mut fraction = &bytes[0..0];
    if bytes.get(cursor) == Some(&b'.') {
        integral = false;
        cursor += 1;
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) { cursor += 1; }
        if cursor == start { return Err(invalid_number(offset)); }
        fraction = bytes.get(start..cursor).ok_or_else(|| internal(offset))?;
    }
    let has_exponent = matches!(bytes.get(cursor), Some(b'e' | b'E'));
    let mut exponent = 0i32;
    if has_exponent {
        integral = false;
        cursor += 1;
        let negative_exponent = bytes.get(cursor) == Some(&b'-');
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) { cursor += 1; }
        let start = cursor;
        while let Some(digit @ b'0'..=b'9') = bytes.get(cursor).copied() {
            exponent = exponent.checked_mul(10)
                .and_then(|value| value.checked_add(i32::from(digit - b'0')))
                .filter(|value| *value <= i32::from(policy.max_exponent_abs))
                .ok_or_else(|| invalid_number(offset))?;
            cursor += 1;
        }
        if cursor == start { return Err(invalid_number(offset)); }
        if negative_exponent { exponent = -exponent; }
    }
    if cursor != bytes.len() { return Err(invalid_number(offset)); }
    if policy.reject_negative_zero && negative
        && integer.iter().chain(fraction).all(|digit| *digit == b'0')
    {
        return Err(vm_error(ErrorCode::InvalidNumber, offset, "negative zero is forbidden"));
    }
    Ok(NumberGrammar { negative, integral, integer, fraction, exponent, has_exponent })
}

/// Called only after the complete integer grammar has been checked.
fn integer(bytes: &[u8], negative: bool, offset: u32) -> Result<SlotValue> {
    if negative {
        let mut value = 0i64;
        for &digit in bytes.get(1..).ok_or_else(|| internal(offset))? {
            value = value.checked_mul(10)
                .and_then(|value| value.checked_sub(i64::from(digit - b'0')))
                .ok_or_else(|| overflow(offset))?;
        }
        Ok(SlotValue::I64(value))
    } else {
        let mut value = 0u64;
        for &digit in bytes {
            value = value.checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(digit - b'0')))
                .ok_or_else(|| overflow(offset))?;
        }
        Ok(SlotValue::U64(value))
    }
}

fn float(bytes: &[u8], policy: &NumericPolicy, offset: u32) -> Result<SlotValue> {
    let text = core::str::from_utf8(bytes).map_err(|_| invalid_number(offset))?;
    let value = text.parse::<f64>().map_err(|_| overflow(offset))?;
    if policy.reject_non_finite_float && !value.is_finite() { return Err(overflow(offset)); }
    // Also reject a negative nonzero lexeme that underflows to negative zero.
    if policy.reject_negative_zero && value == 0.0 && value.is_sign_negative() {
        return Err(vm_error(ErrorCode::InvalidNumber, offset, "negative zero is forbidden"));
    }
    Ok(SlotValue::F64Bits(value.to_bits()))
}

fn decimal(
    text: &str,
    constraints: &DecimalConstraints,
    numeric: &NumericPolicy,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<Decimal128> {
    let policy = &constraints.policy;
    jmax_decimal::validate_policy(policy).map_err(|error| at_token(error, offset))?;
    let lexeme = number_grammar(text.as_bytes(), numeric, budget, offset)?;
    if lexeme.has_exponent && !policy.allow_exponent { return Err(invalid_number(offset)); }
    // Accumulate decoded digits directly, so JSON escapes need no temporary
    // quoted allocation. Signed accumulation includes i128::MIN exactly.
    let mut coefficient = 0i128;
    for &digit in lexeme.integer.iter().chain(lexeme.fraction) {
        let digit = i128::from(digit - b'0');
        let digit = if lexeme.negative { -digit } else { digit };
        coefficient = coefficient.checked_mul(10).and_then(|value| value.checked_add(digit))
            .ok_or_else(|| overflow(offset))?;
    }
    // Rescaling and exact comparisons have bounded decimal-digit work, even
    // for extreme scales. Prepay their fixed upper bound before calling them.
    budget.charge(2_048, offset)?;
    let accumulated = jmax_decimal::AccumulatedDecimal {
        coefficient,
        fractional_digits: u32::try_from(lexeme.fraction.len()).map_err(|_| resource(offset))?,
        exponent: lexeme.exponent,
        precision: jmax_decimal::coefficient_precision(coefficient),
    };
    let value = jmax_decimal::bounds::rescale(accumulated, policy, offset)
        .map_err(|error| at_token(error, offset))?;
    jmax_decimal::bounds::validate(value, policy, jmax_decimal::DecimalBounds {
        min: constraints.minimum, max: constraints.maximum,
    }, offset).map_err(|error| at_token(error, offset))?;
    Ok(value)
}

/// Verify JSON escapes and compare decoded scalars without allocating storage.
fn check_string_token(
    token: &[u8],
    decoded: &str,
    reference: StrRef,
    span: Span32,
    offset: u32,
) -> Result<()> {
    if token.len() < 2 || token.first() != Some(&b'"') || token.last() != Some(&b'"') {
        return Err(vm_error(ErrorCode::InvalidSyntax, offset, "string token lacks quotes"));
    }
    if reference.source == SpanSource::RawPayload {
        let start = span.offset.checked_add(1).ok_or_else(|| internal(offset))?;
        let len = span.len.checked_sub(2).ok_or_else(|| internal(offset))?;
        if reference.span != Span32::new(start, len) { return Err(internal(offset)); }
    }
    let interior = token.get(1..token.len() - 1).ok_or_else(|| internal(offset))?;
    let interior = core::str::from_utf8(interior)
        .map_err(|_| vm_error(ErrorCode::InvalidUtf8, offset, "raw string is not UTF-8"))?;
    let mut raw = interior.chars();
    let mut expected = decoded.chars();
    while let Some(scalar) = raw.next() {
        let scalar = match scalar {
            '"' => return Err(vm_error(ErrorCode::InvalidSyntax, offset, "unescaped quote in string")),
            '\u{0}'..='\u{1f}' => return Err(vm_error(ErrorCode::ControlCharInString, offset,
                "unescaped control character in string")),
            '\\' => {
                if reference.source == SpanSource::RawPayload { return Err(internal(offset)); }
                match raw.next() {
                    Some('"') => '"', Some('\\') => '\\', Some('/') => '/',
                    Some('b') => '\u{8}', Some('f') => '\u{c}', Some('n') => '\n',
                    Some('r') => '\r', Some('t') => '\t',
                    Some('u') => {
                        let first = hex_quad(&mut raw, offset)?;
                        let scalar = match first {
                            0xd800..=0xdbff => {
                                if raw.next() != Some('\\') || raw.next() != Some('u') {
                                    return Err(lone_surrogate(offset));
                                }
                                let second = hex_quad(&mut raw, offset)?;
                                if !(0xdc00..=0xdfff).contains(&second) { return Err(lone_surrogate(offset)); }
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            }
                            0xdc00..=0xdfff => return Err(lone_surrogate(offset)),
                            _ => first,
                        };
                        char::from_u32(scalar).ok_or_else(|| lone_surrogate(offset))?
                    }
                    _ => return Err(vm_error(ErrorCode::InvalidEscape, offset, "invalid JSON escape")),
                }
            }
            scalar => scalar,
        };
        if expected.next() != Some(scalar) { return Err(internal(offset)); }
    }
    if expected.next().is_some() { return Err(internal(offset)); }
    Ok(())
}

fn hex_quad(chars: &mut core::str::Chars<'_>, offset: u32) -> Result<u32> {
    let mut value = 0u32;
    for _ in 0..4 {
        let digit = chars.next().and_then(|scalar| scalar.to_digit(16))
            .ok_or_else(|| vm_error(ErrorCode::InvalidEscape, offset, "invalid Unicode escape"))?;
        // Exactly four hexadecimal digits, so this cannot exceed u16::MAX.
        value = value * 16 + digit;
    }
    Ok(value)
}

fn charge_bytes(budget: &mut VmBudget, len: usize, passes: u64, offset: u32) -> Result<()> {
    let units = u64::try_from(len).ok().and_then(|len| len.checked_mul(passes))
        .ok_or_else(|| resource(offset))?;
    budget.charge(units, offset)
}

fn at_token(error: Error, offset: u32) -> Error { Error { byte_offset: offset, ..error } }
fn internal(offset: u32) -> Error {
    vm_error(ErrorCode::InternalError, offset, "inconsistent scalar metadata or validated program")
}
fn resource(offset: u32) -> Error {
    vm_error(ErrorCode::BudgetExhausted, offset, "scalar constraint resource limit exceeded")
}
fn invalid_number(offset: u32) -> Error {
    vm_error(ErrorCode::InvalidNumber, offset, "invalid number grammar or exponent policy")
}
fn overflow(offset: u32) -> Error {
    vm_error(ErrorCode::NumberOverflow, offset, "number is outside its representation range")
}
fn type_error(offset: u32) -> Error {
    vm_error(ErrorCode::TypeMismatch, offset, "scalar does not match the schema kind")
}
fn lone_surrogate(offset: u32) -> Error {
    vm_error(ErrorCode::LoneSurrogate, offset, "Unicode escape contains an unpaired surrogate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec};
    use jmax_abi::number::DecimalPolicy;
    use jmax_format::{FormatId, FormatOptions, HexCase, Padding};
    use jmax_schema_compile::{CompilerOptions, PatternOp, PATTERN_VERSION};
    use jmax_schema_ir::{ResolveLimits, ScalarConstraints, Schema, SchemaId, TypeId, TypeNode};

    const OFFSET: u32 = 3;

    fn budget() -> VmBudget { VmBudget::new(VmLimits::default()) }

    fn limited(steps: u64, scratch: u32) -> VmBudget {
        VmBudget::new(VmLimits { max_steps: steps, max_scratch_bytes: scratch, ..VmLimits::default() })
    }

    fn program(node: TypeNode, numeric: NumericPolicy) -> ValidatedProgram {
        let schema = Schema {
            name: String::from("constraint-vectors"), schema_id: SchemaId(318),
            root: TypeId(0), nodes: vec![node], definitions: Vec::new(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        let mut options = CompilerOptions::default();
        options.policy.numeric = numeric;
        jmax_schema_compile::lower::compile(&resolved, &options).unwrap()
    }

    fn scalar_program(kind: ValueKind) -> ValidatedProgram {
        program(TypeNode::Scalar(ScalarConstraints::new(kind)), NumericPolicy::strict_default())
    }

    fn any_program() -> ValidatedProgram {
        program(TypeNode::Any, NumericPolicy::strict_default())
    }

    fn evaluate(
        program: &ValidatedProgram,
        state: Option<StateId>,
        kind: ScalarKind,
        token: &[u8],
        decoded: Option<&str>,
        budget: &mut VmBudget,
    ) -> Result<SlotValue> {
        let mut raw = b"   ".to_vec();
        raw.extend_from_slice(token);
        let span = Span32::new(OFFSET, u32::try_from(token.len()).unwrap());
        let arena = decoded.unwrap_or("").as_bytes();
        let reference = if kind == ScalarKind::String {
            Some(match decoded {
                Some(text) => StrRef {
                    span: Span32::new(0, u32::try_from(text.len()).unwrap()),
                    source: SpanSource::DecodedArena,
                    scalar_len: u32::try_from(text.chars().count()).unwrap(),
                },
                None => {
                    let interior = core::str::from_utf8(&token[1..token.len() - 1]).unwrap();
                    StrRef {
                        span: Span32::new(OFFSET + 1, u32::try_from(interior.len()).unwrap()),
                        source: SpanSource::RawPayload,
                        scalar_len: u32::try_from(interior.chars().count()).unwrap(),
                    }
                }
            })
        } else { None };
        check_scalar(program, state, ScalarInput { kind, span, decoded: reference, raw: &raw, arena }, budget)
    }

    fn eval(program: &ValidatedProgram, kind: ScalarKind, token: &[u8]) -> Result<SlotValue> {
        evaluate(program, Some(StateId(0)), kind, token, None, &mut budget())
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code, "{error:?}");
        assert_eq!(error.byte_offset, offset, "{error:?}");
        error
    }

    #[test]
    fn count_bounds_are_inclusive_at_zero_and_u32_max() {
        let mut constraints = ConstraintSet::default();
        for count in [0, 1, u32::MAX] { check_count(&constraints, count, 99).unwrap(); }
        constraints.min_count = Some(2);
        constraints.max_count = Some(4);
        for count in [2, 3, 4] { check_count(&constraints, count, 99).unwrap(); }
        for count in [0, 1, 5, u32::MAX] {
            let error = assert_error(check_count(&constraints, count, 99), ErrorCode::ConstraintViolation, 99);
            assert_eq!(error.source, ErrorSource::SchemaVm);
        }
        for bound in [0, u32::MAX] {
            constraints.min_count = Some(bound);
            constraints.max_count = Some(bound);
            check_count(&constraints, bound, u32::MAX).unwrap();
            assert_error(check_count(&constraints, bound ^ 1, u32::MAX),
                ErrorCode::ConstraintViolation, u32::MAX);
        }
        constraints.min_count = Some(2);
        constraints.max_count = Some(1);
        for count in [0, 1, 2, 3] {
            assert_error(check_count(&constraints, count, 9), ErrorCode::ConstraintViolation, 9);
        }
    }

    #[test]
    fn null_and_booleans_have_present_slots_and_exact_lexemes() {
        let any = any_program();
        for state in [None, Some(StateId(0))] {
            for (kind, token, expected) in [
                (ScalarKind::Null, &b"null"[..], SlotValue::RawSpan(Span32::new(OFFSET, 4))),
                (ScalarKind::True, &b"true"[..], SlotValue::Bool(true)),
                (ScalarKind::False, &b"false"[..], SlotValue::Bool(false)),
            ] {
                assert_eq!(evaluate(&any, state, kind, token, None, &mut budget()).unwrap(), expected);
                assert_ne!(expected, SlotValue::Empty);
            }
        }
        let null = scalar_program(ValueKind::Null);
        assert!(matches!(eval(&null, ScalarKind::Null, b"null").unwrap(), SlotValue::RawSpan(_)));
        assert_error(eval(&null, ScalarKind::True, b"true"), ErrorCode::TypeMismatch, OFFSET);
        let boolean = scalar_program(ValueKind::Boolean);
        assert_eq!(eval(&boolean, ScalarKind::False, b"false").unwrap(), SlotValue::Bool(false));
        for (kind, token) in [(ScalarKind::Null, &b"NULL"[..]), (ScalarKind::True, b"false"),
            (ScalarKind::False, b"false "), (ScalarKind::Null, b"")]
        {
            assert_error(eval(&any, kind, token), ErrorCode::InvalidSyntax, OFFSET);
        }
    }

    #[test]
    fn integers_preserve_sign_and_full_abi_endpoints() {
        let program = scalar_program(ValueKind::Integer);
        for (token, expected) in [
            (&b"0"[..], SlotValue::U64(0)), (b"1", SlotValue::U64(1)),
            (b"-1", SlotValue::I64(-1)),
            (b"9223372036854775807", SlotValue::U64(i64::MAX as u64)),
            (b"9223372036854775808", SlotValue::U64(1u64 << 63)),
            (b"18446744073709551615", SlotValue::U64(u64::MAX)),
            (b"-9223372036854775808", SlotValue::I64(i64::MIN)),
        ] {
            assert_eq!(eval(&program, ScalarKind::Number, token).unwrap(), expected);
        }
        for token in [&b"18446744073709551616"[..], b"-9223372036854775809"] {
            assert_error(eval(&program, ScalarKind::Number, token), ErrorCode::NumberOverflow, OFFSET);
        }
        for token in [&b"1.0"[..], b"1e0", b"0E+0"] {
            assert_error(eval(&program, ScalarKind::Number, token), ErrorCode::TypeMismatch, OFFSET);
        }
    }

    #[test]
    fn integer_bounds_never_narrow_unsigned_values() {
        let mut constraints = ConstraintSet { min_i64: Some(i64::MIN), max_i64: Some(i64::MAX),
            ..ConstraintSet::default() };
        for value in [SlotValue::I64(i64::MIN), SlotValue::I64(-1), SlotValue::U64(0),
            SlotValue::U64(i64::MAX as u64)]
        {
            integer_bounds(value, &constraints, 21).unwrap();
        }
        for value in [SlotValue::U64(1u64 << 63), SlotValue::U64(u64::MAX)] {
            assert_error(integer_bounds(value, &constraints, 21), ErrorCode::ConstraintViolation, 21);
        }
        constraints.max_i64 = Some(-1);
        assert_error(integer_bounds(SlotValue::U64(0), &constraints, 21), ErrorCode::ConstraintViolation, 21);
        constraints.min_i64 = Some(0);
        constraints.max_i64 = None;
        integer_bounds(SlotValue::U64(u64::MAX), &constraints, 21).unwrap();
        assert_error(integer_bounds(SlotValue::I64(-1), &constraints, 21), ErrorCode::ConstraintViolation, 21);
        let mut scalar = ScalarConstraints::new(ValueKind::Integer);
        scalar.min_i64 = Some(-2);
        scalar.max_i64 = Some(2);
        let program = program(TypeNode::Scalar(scalar), NumericPolicy::strict_default());
        for token in [&b"-2"[..], b"2"] { eval(&program, ScalarKind::Number, token).unwrap(); }
        for token in [&b"-3"[..], b"3"] {
            assert_error(eval(&program, ScalarKind::Number, token), ErrorCode::ConstraintViolation, OFFSET);
        }
    }

    #[test]
    fn invalid_number_vectors_are_rejected_before_conversion() {
        let program = any_program();
        for token in [
            &b"+1"[..], b"01", b"-01", b".1", b"1.", b"1e", b"1e+", b"1e-",
            b"--1", b"-", b"1x", b" 1", b"1 ", b"NaN", b"Infinity", b"0x10",
            b"1_000", b"1\0", b"1e1.0", b"1E++2", "١".as_bytes(), b"1e309",
            b"1e-309", b"1e999999999999999999999999999999999999",
        ] {
            let error = assert_error(eval(&program, ScalarKind::Number, token), ErrorCode::InvalidNumber, OFFSET);
            assert_eq!(error.source, ErrorSource::SchemaVm);
        }
        for token in [&b"1e308"[..], b"1e-308", b"1e00000000000000000000000000000000000000001"] {
            assert!(matches!(eval(&program, ScalarKind::Number, token).unwrap(), SlotValue::F64Bits(_)));
        }
    }

    #[test]
    fn floating_conversion_and_any_integer_fallback_are_explicit() {
        let number = scalar_program(ValueKind::Number);
        for (token, expected) in [(&b"1"[..], 1.0f64), (b"1.5", 1.5), (b"-2e2", -200.0)] {
            assert_eq!(eval(&number, ScalarKind::Number, token).unwrap(), SlotValue::F64Bits(expected.to_bits()));
        }
        let any = any_program();
        assert_eq!(eval(&any, ScalarKind::Number, b"1").unwrap(), SlotValue::U64(1));
        assert_eq!(eval(&any, ScalarKind::Number, b"1.0").unwrap(), SlotValue::F64Bits(1.0f64.to_bits()));
        assert_eq!(eval(&any, ScalarKind::Number, b"18446744073709551616").unwrap(),
            SlotValue::F64Bits(18446744073709551616.0f64.to_bits()));
        assert!(matches!(eval(&any, ScalarKind::Number, b"-9223372036854775809").unwrap(), SlotValue::F64Bits(_)));
        assert_error(eval(&any, ScalarKind::Number, b"9e308"), ErrorCode::NumberOverflow, OFFSET);
        let mut policy = NumericPolicy::strict_default();
        policy.reject_non_finite_float = false;
        let permissive = program(TypeNode::Any, policy);
        assert_eq!(eval(&permissive, ScalarKind::Number, b"9e308").unwrap(),
            SlotValue::F64Bits(f64::INFINITY.to_bits()));
    }

    #[test]
    fn negative_zero_policy_covers_integer_fraction_exponent_and_underflow() {
        let strict = any_program();
        for token in [&b"-0"[..], b"-0.00", b"-0e2", b"-0.00000000000000000001e-308"] {
            assert_error(eval(&strict, ScalarKind::Number, token), ErrorCode::InvalidNumber, OFFSET);
        }
        let mut policy = NumericPolicy::strict_default();
        policy.reject_negative_zero = false;
        let relaxed = program(TypeNode::Any, policy);
        assert_eq!(eval(&relaxed, ScalarKind::Number, b"-0").unwrap(), SlotValue::I64(0));
        for token in [&b"-0.0"[..], b"-0e0"] {
            assert_eq!(eval(&relaxed, ScalarKind::Number, token).unwrap(), SlotValue::F64Bits((-0.0f64).to_bits()));
        }
        policy.max_exponent_abs = 0;
        let no_exponent_magnitude = program(TypeNode::Any, policy);
        eval(&no_exponent_magnitude, ScalarKind::Number, b"1e-00000").unwrap();
        assert_error(eval(&no_exponent_magnitude, ScalarKind::Number, b"1e1"), ErrorCode::InvalidNumber, OFFSET);
    }

    #[test]
    fn quoted_integers_require_opt_in_and_strict_decoded_grammar() {
        let strict = scalar_program(ValueKind::Integer);
        assert_error(eval(&strict, ScalarKind::String, b"\"12\""), ErrorCode::TypeMismatch, OFFSET);
        let mut policy = NumericPolicy::strict_default();
        policy.allow_Quoted_int = true;
        let program = program(TypeNode::Scalar(ScalarConstraints::new(ValueKind::Integer)), policy);
        assert_eq!(eval(&program, ScalarKind::String, b"\"18446744073709551615\"").unwrap(), SlotValue::U64(u64::MAX));
        assert_eq!(evaluate(&program, Some(StateId(0)), ScalarKind::String,
            b"\"\\u0031\\u0032\"", Some("12"), &mut budget()).unwrap(), SlotValue::U64(12));
        for token in [&b"\"+1\""[..], b"\"01\"", b"\" 1\"", b"\"1e\"", b"\"-0\""] {
            assert_error(eval(&program, ScalarKind::String, token), ErrorCode::InvalidNumber, OFFSET);
        }
        for token in [&b"\"1.0\""[..], b"\"1e0\""] {
            assert_error(eval(&program, ScalarKind::String, token), ErrorCode::TypeMismatch, OFFSET);
        }
    }

    #[test]
    fn string_lengths_count_unicode_scalars_and_preserve_owner_references() {
        let mut constraints = ScalarConstraints::new(ValueKind::String);
        constraints.min_length = Some(2);
        constraints.max_length = Some(2);
        let program = program(TypeNode::Scalar(constraints), NumericPolicy::strict_default());
        let raw = eval(&program, ScalarKind::String, "\"é😀\"".as_bytes()).unwrap();
        assert_eq!(raw, SlotValue::Str(StrRef { span: Span32::new(OFFSET + 1, 6),
            source: SpanSource::RawPayload, scalar_len: 2 }));
        let escaped = evaluate(&program, Some(StateId(0)), ScalarKind::String,
            b"\"\\u00e9\\ud83d\\ude00\"", Some("é😀"), &mut budget()).unwrap();
        assert_eq!(escaped, SlotValue::Str(StrRef { span: Span32::new(0, 6),
            source: SpanSource::DecodedArena, scalar_len: 2 }));
        // Combining marks count separately; no normalization takes place.
        eval(&program, ScalarKind::String, "\"e\u{301}\"".as_bytes()).unwrap();
        for token in [&b"\"\""[..], "\"😀\"".as_bytes(), "\"é😀a\"".as_bytes()] {
            assert_error(eval(&program, ScalarKind::String, token), ErrorCode::ConstraintViolation, OFFSET);
        }
    }

    #[test]
    fn strings_validate_all_escapes_and_reject_mismatched_decoding() {
        let program = scalar_program(ValueKind::String);
        evaluate(&program, Some(StateId(0)), ScalarKind::String,
            br#""\"\\\/\b\f\n\r\t\u0000""#, Some("\"\\/\u{8}\u{c}\n\r\t\0"), &mut budget()).unwrap();
        for (token, decoded, code) in [
            (&b"\"\\q\""[..], "x", ErrorCode::InvalidEscape),
            (b"\"\\u00xz\"", "x", ErrorCode::InvalidEscape),
            (b"\"\\ud800\"", "x", ErrorCode::LoneSurrogate),
            (b"\"\\udc00\"", "x", ErrorCode::LoneSurrogate),
            (b"\"\\ud800\\u0041\"", "x", ErrorCode::LoneSurrogate),
            (b"\"a\nb\"", "a\nb", ErrorCode::ControlCharInString),
            (b"\"a\"b\"", "a\"b", ErrorCode::InvalidSyntax),
            (b"\"abc\"", "abd", ErrorCode::InternalError),
            (b"\"ab\"", "abc", ErrorCode::InternalError),
            (b"\"abc\"", "ab", ErrorCode::InternalError),
            (b"\"\\u0031\"", "2", ErrorCode::InternalError),
        ] {
            assert_error(evaluate(&program, Some(StateId(0)), ScalarKind::String, token,
                Some(decoded), &mut budget()), code, OFFSET);
        }
    }

    #[test]
    fn invalid_spans_metadata_and_state_ids_fail_without_publishing_values() {
        let program = any_program();
        let base = ScalarInput { kind: ScalarKind::String, span: Span32::new(0, 3),
            decoded: Some(StrRef { span: Span32::new(1, 1), source: SpanSource::RawPayload, scalar_len: 1 }),
            raw: b"\"x\"", arena: b"" };
        for input in [
            ScalarInput { span: Span32::new(u32::MAX, 2), ..base },
            ScalarInput { span: Span32::new(0, 4), ..base },
            ScalarInput { decoded: None, ..base },
            ScalarInput { kind: ScalarKind::Null, ..base },
            ScalarInput { decoded: Some(StrRef { scalar_len: 2, ..base.decoded.unwrap() }), ..base },
            ScalarInput { decoded: Some(StrRef { span: Span32::new(0, 1), ..base.decoded.unwrap() }), ..base },
            ScalarInput { decoded: Some(StrRef { source: SpanSource::DecodedArena, ..base.decoded.unwrap() }), ..base },
        ] {
            assert_error(check_scalar(&program, None, input, &mut budget()), ErrorCode::InternalError, input.span.offset);
        }
        assert_error(check_scalar(&program, Some(StateId(u32::MAX)), base, &mut budget()), ErrorCode::InternalError, 0);
        let invalid_utf8 = ScalarInput { arena: &[0xff], decoded: Some(StrRef {
            span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1,
        }), ..base };
        assert_error(check_scalar(&program, None, invalid_utf8, &mut budget()), ErrorCode::InvalidUtf8, 0);
        let array = program_array();
        assert_error(eval(&array, ScalarKind::Null, b"null"), ErrorCode::TypeMismatch, OFFSET);
    }

    fn program_array() -> ValidatedProgram {
        let schema = Schema { name: String::from("array"), schema_id: SchemaId(318), root: TypeId(0),
            nodes: vec![TypeNode::Array(jmax_schema_ir::ArrayShape { items: TypeId(1),
                min_items: None, max_items: None }), TypeNode::Any], definitions: Vec::new() };
        let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits::default()).unwrap();
        jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default()).unwrap()
    }

    fn decimal_constraints() -> DecimalConstraints {
        DecimalConstraints {
            policy: DecimalPolicy { max_precision: 6, target_scale: 2, allow_exponent: false,
                reject_inexact_rescale: true, tick_step: 5 },
            minimum: Some(Decimal128::new(-2, 0)), maximum: Some(Decimal128::new(2000, 3)),
        }
    }

    #[test]
    fn decimals_use_exact_scale_bounds_precision_and_ticks() {
        let constraints = decimal_constraints();
        let numeric = NumericPolicy::strict_default();
        for (text, coefficient) in [("-2", -200), ("2.00", 200), ("1.05", 105), ("0", 0), ("1.0500", 105)] {
            assert_eq!(decimal(text, &constraints, &numeric, &mut budget(), 17).unwrap(), Decimal128::new(coefficient, 2));
        }
        for (text, code) in [
            ("2.05", ErrorCode::ConstraintViolation), ("-2.05", ErrorCode::ConstraintViolation),
            ("1.01", ErrorCode::TickMultipleViolation), ("-1.01", ErrorCode::TickMultipleViolation),
            ("1.001", ErrorCode::DecimalScaleMismatch), ("1e0", ErrorCode::InvalidNumber),
            ("01", ErrorCode::InvalidNumber), ("+1", ErrorCode::InvalidNumber),
            ("-0.00", ErrorCode::InvalidNumber),
            ("170141183460469231731687303715884105728", ErrorCode::NumberOverflow),
        ] {
            assert_error(decimal(text, &constraints, &numeric, &mut budget(), 17), code, 17);
        }
        let mut tight = constraints.clone();
        tight.policy.max_precision = 2;
        assert_error(decimal("1", &tight, &numeric, &mut budget(), 17), ErrorCode::DecimalPrecisionOverflow, 17);
        tight.policy = DecimalPolicy { max_precision: 38, target_scale: 0, allow_exponent: true,
            reject_inexact_rescale: true, tick_step: 1 };
        tight.minimum = None;
        tight.maximum = None;
        assert_error(decimal("1e308", &tight, &numeric, &mut budget(), 17), ErrorCode::NumberOverflow, 17);
        assert_error(decimal("1e-308", &tight, &numeric, &mut budget(), 17), ErrorCode::DecimalScaleMismatch, 17);
    }

    #[test]
    fn decimal_optional_truncation_and_full_signed_accumulation_are_deterministic() {
        let mut constraints = decimal_constraints();
        let numeric = NumericPolicy::strict_default();
        constraints.policy.reject_inexact_rescale = false;
        for (text, coefficient) in [("1.059", 105), ("-1.059", -105)] {
            assert_eq!(decimal(text, &constraints, &numeric, &mut budget(), 0).unwrap(), Decimal128::new(coefficient, 2));
        }
        constraints.policy = DecimalPolicy { max_precision: 39, target_scale: 0,
            allow_exponent: true, reject_inexact_rescale: true, tick_step: 1 };
        constraints.minimum = None;
        constraints.maximum = None;
        for (text, coefficient) in [("170141183460469231731687303715884105727", i128::MAX),
            ("-170141183460469231731687303715884105728", i128::MIN), ("12e2", 1200)]
        {
            assert_eq!(decimal(text, &constraints, &numeric, &mut budget(), 0).unwrap(), Decimal128::new(coefficient, 0));
        }
        constraints.policy.tick_step = 0;
        decimal("17", &constraints, &numeric, &mut budget(), 0).unwrap();
        constraints.policy.tick_step = -1;
        assert_error(decimal("17", &constraints, &numeric, &mut budget(), 23), ErrorCode::ConstraintViolation, 23);
    }

    #[test]
    fn decimal_schema_accepts_decoded_strings_and_rejects_unquoted_numbers() {
        let mut scalar = ScalarConstraints::new(ValueKind::Decimal);
        scalar.decimal = Some(decimal_constraints());
        let program = program(TypeNode::Scalar(scalar), NumericPolicy::strict_default());
        assert_eq!(eval(&program, ScalarKind::String, b"\"1.05\"").unwrap(), SlotValue::Decimal(Decimal128::new(105, 2)));
        assert_eq!(evaluate(&program, Some(StateId(0)), ScalarKind::String,
            b"\"\\u0031.05\"", Some("1.05"), &mut budget()).unwrap(), SlotValue::Decimal(Decimal128::new(105, 2)));
        assert_error(eval(&program, ScalarKind::Number, b"1.05"), ErrorCode::TypeMismatch, OFFSET);
        assert_error(evaluate(&program, Some(StateId(0)), ScalarKind::String,
            b"\"\\u0031.001\"", Some("1.001"), &mut budget()), ErrorCode::DecimalScaleMismatch, OFFSET);
    }

    #[test]
    fn pattern_vectors_cover_unicode_anchors_empty_matches_and_malformed_programs() {
        for (source, text, matches) in [
            ("^é😀$", "é😀", true), ("^é😀$", "xé😀", false),
            ("a+", "xxaa", true), ("^a$", "a\n", false), ("", "", true),
            ("a*", "bbb", true), ("^.$", "😀", true), ("^.$", "\n", false),
            ("^[a-c]+$", "abc", true), ("^[a-c]+$", "abd", false),
        ] {
            let pattern = jmax_schema_compile::patterns::compile(source, 128, 100_000).unwrap();
            let mut work = budget();
            work.retain(7, 0).unwrap();
            let before = work.steps_left();
            let result = check_pattern(&pattern, text, &mut work, 41);
            if matches { result.unwrap(); }
            else { assert_error(result, ErrorCode::PatternMismatch, 41); }
            assert!(work.steps_left() < before);
            assert_eq!(work.scratch_bytes(), 7);
        }
        let malformed = PatternProgram { version: PATTERN_VERSION, start: 0,
            ops: vec![PatternOp::Jump { next: 99 }], max_steps: 100 };
        let mut work = budget();
        assert_error(check_pattern(&malformed, "", &mut work, 41), ErrorCode::SchemaImageInvalid, 41);
        assert_eq!(work.scratch_bytes(), 0);
    }

    #[test]
    fn patterns_use_shared_steps_local_limits_and_bounded_scratch() {
        let pattern = jmax_schema_compile::patterns::compile("^a+$", 128, 100_000).unwrap();
        let mut work = budget();
        check_pattern(&pattern, "aaaa", &mut work, 7).unwrap();
        let after_first = work.steps_left();
        check_pattern(&pattern, "aaaa", &mut work, 7).unwrap();
        assert!(work.steps_left() < after_first);
        let mut empty = limited(0, u32::MAX);
        assert_error(check_pattern(&pattern, "aaaa", &mut empty, 7), ErrorCode::BudgetExhausted, 7);
        let mut no_scratch = limited(u64::MAX, 0);
        assert_error(check_pattern(&pattern, "aaaa", &mut no_scratch, 7), ErrorCode::BudgetExhausted, 7);
        assert_eq!(no_scratch.scratch_bytes(), 0);
        let mut locally_limited = pattern;
        locally_limited.max_steps = 1;
        let mut work = budget();
        assert_error(check_pattern(&locally_limited, "aaaa", &mut work, 7), ErrorCode::BudgetExhausted, 7);
        assert_eq!(work.scratch_bytes(), 0);
    }

    #[test]
    fn format_dispatch_honors_options_and_raw_token_offsets() {
        let mut options = FormatOptions::default();
        for (id, text) in [
            (FormatId::Uuid, "00000000-0000-0000-0000-000000000000"),
            (FormatId::Ipv4, "127.0.0.1"), (FormatId::Ipv6, "::1"),
            (FormatId::IpAddr, "192.0.2.1"), (FormatId::Base64, "Zg=="),
            (FormatId::Base64Url, "_w=="), (FormatId::Hex, "abcd"),
            (FormatId::NanoId, "0123456789abcdefghijk"),
            (FormatId::BlockchainAddress, "0x0000000000000000000000000000000000000000"),
            (FormatId::BlockchainTransactionHash, "0x0000000000000000000000000000000000000000000000000000000000000000"),
        ] { check_format(id, text, &options, &mut budget(), 83).unwrap(); }
        options.hex.case = HexCase::Lower;
        assert_error(check_format(FormatId::Hex, "AB", &options, &mut budget(), 83), ErrorCode::FormatMismatch, 83);
        options.hex.prefix = true;
        options.hex.exact_bytes = Some(1);
        check_format(FormatId::Hex, "0xab", &options, &mut budget(), 83).unwrap();
        options.nanoid_len = 2;
        check_format(FormatId::NanoId, "_-", &options, &mut budget(), 83).unwrap();
        options.base64_padding = Padding::Forbidden;
        check_format(FormatId::Base64, "Zg", &options, &mut budget(), 83).unwrap();
        assert_error(check_format(FormatId::Base64, "Zh", &options, &mut budget(), 83), ErrorCode::FormatMismatch, 83);
        options.max_input_bytes = 1;
        assert_error(check_format(FormatId::Hex, "ab", &options, &mut budget(), 83), ErrorCode::BudgetExhausted, 83);
        let mut scalar = ScalarConstraints::new(ValueKind::String);
        scalar.format = Some(String::from("ipv4"));
        let program = program(TypeNode::Scalar(scalar), NumericPolicy::strict_default());
        let error = assert_error(evaluate(&program, Some(StateId(0)), ScalarKind::String,
            b"\"\\u0039\\u0039\\u0039.1.1.1\"", Some("999.1.1.1"), &mut budget()),
            ErrorCode::FormatMismatch, OFFSET);
        assert_eq!(error.source, ErrorSource::SchemaVm);
    }

    #[test]
    fn combined_string_constraints_run_in_length_pattern_format_order() {
        let mut scalar = ScalarConstraints::new(ValueKind::String);
        scalar.min_length = Some(4);
        scalar.pattern = Some(String::from("^[0-9a-z]+$"));
        scalar.format = Some(String::from("hex"));
        let program = program(TypeNode::Scalar(scalar), NumericPolicy::strict_default());
        eval(&program, ScalarKind::String, b"\"abcd\"").unwrap();
        for (token, code) in [(&b"\"!\""[..], ErrorCode::ConstraintViolation),
            (b"\"ABCD\"", ErrorCode::PatternMismatch), (b"\"zzzz\"", ErrorCode::FormatMismatch)]
        { assert_error(eval(&program, ScalarKind::String, token), code, OFFSET); }
    }

    #[test]
    fn scalar_and_format_work_exhaustion_never_returns_a_slot_or_refunds_steps() {
        let program = any_program();
        let mut work = limited(0, 0);
        assert_error(evaluate(&program, None, ScalarKind::Null, b"null", None, &mut work),
            ErrorCode::BudgetExhausted, OFFSET);
        let mut work = limited(1, 0);
        assert_error(evaluate(&program, None, ScalarKind::Number, b"123", None, &mut work),
            ErrorCode::BudgetExhausted, OFFSET);
        assert_eq!(work.steps_left(), 0);
        let mut work = limited(17, 0);
        check_format(FormatId::Hex, "ab", &FormatOptions::default(), &mut work, 61).unwrap();
        assert_eq!(work.steps_left(), 0);
        assert_error(check_format(FormatId::Hex, "ab", &FormatOptions::default(), &mut work, 61),
            ErrorCode::BudgetExhausted, 61);
        assert_eq!(work.scratch_bytes(), 0);
        let mut work = limited(100, 0);
        assert_error(decimal("1", &decimal_constraints(), &NumericPolicy::strict_default(), &mut work, 61),
            ErrorCode::BudgetExhausted, 61);
        assert!(work.steps_left() < 100);
    }
}
