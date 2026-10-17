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
    check_scalar_inner(program, state, value, budget, false)
}

/// Only the direct engine may use validated BlockCursor metadata here.
pub(crate) fn check_scalar_trusted(
    program: &ValidatedProgram,
    state: Option<StateId>,
    value: ScalarInput<'_>,
    budget: &mut VmBudget,
) -> Result<SlotValue> {
    check_scalar_inner(program, state, value, budget, true)
}

fn check_scalar_inner(
    program: &ValidatedProgram,
    state: Option<StateId>,
    value: ScalarInput<'_>,
    budget: &mut VmBudget,
    trusted: bool,
) -> Result<SlotValue> {
    let offset = value.span.offset;
    budget.charge(1, offset)?;
    let current = match state {
        Some(id) => {
            let index = usize::try_from(id.0).map_err(|_| internal(offset))?;
            let current = program
                .program()
                .states
                .get(index)
                .ok_or_else(|| internal(offset))?;
            if current.id != id {
                return Err(internal(offset));
            }
            Some(current)
        }
        None => None,
    };
    // A nullable state takes a JSON null whatever its kind.
    let null_here = value.kind == ScalarKind::Null && current.is_some_and(|current| current.constraints.nullable);
    let kind = match current.map(|current| &current.kind) {
        None | Some(StateKind::Any) => None,
        _ if null_here => Some(ValueKind::Null),
        Some(StateKind::Scalar(kind)) => Some(*kind),
        Some(StateKind::Object { .. } | StateKind::Array { .. }) => return Err(type_error(offset)),
    };
    let token = value
        .span
        .slice(value.raw)
        .ok_or_else(|| internal(offset))?;
    if token.is_empty() {
        return Err(vm_error(
            ErrorCode::InvalidSyntax,
            offset,
            "empty scalar token",
        ));
    }
    let numeric = &program.program().policy.numeric;
    let text = match (value.kind, value.decoded) {
        (ScalarKind::String, Some(reference)) => {
            // Raw event sources remain defensive; direct cursor metadata has
            // already paid for lexical validation and exact scalar counting.
            if !trusted {
                charge_bytes(budget, token.len(), 2, offset)?;
                let decoded_len =
                    usize::try_from(reference.span.len).map_err(|_| internal(offset))?;
                charge_bytes(budget, decoded_len, 3, offset)?;
            }
            let text = if trusted {
                let bytes = reference
                    .span
                    .slice(match reference.source {
                        SpanSource::RawPayload => value.raw,
                        SpanSource::DecodedArena => value.arena,
                    })
                    .ok_or_else(|| internal(offset))?;
                // SAFETY: only the direct engine passes trusted=true, with a
                // private StrRef produced by BlockCursor on these exact buffers.
                unsafe { core::str::from_utf8_unchecked(bytes) }
            } else {
                let text = resolve_str(value.raw, value.arena, reference)
                    .map_err(|error| at_token(error, offset))?;
                check_string_token(token, text, reference, value.span, offset)?;
                text
            };
            Some(text)
        }
        (ScalarKind::String, None) | (_, Some(_)) => return Err(internal(offset)),
        (ScalarKind::Null, None) if token == b"null" => None,
        (ScalarKind::True, None) if token == b"true" => None,
        (ScalarKind::False, None) if token == b"false" => None,
        (ScalarKind::Number, None) => None,
        _ => {
            return Err(vm_error(
                ErrorCode::InvalidSyntax,
                offset,
                "scalar kind disagrees with its raw token",
            ));
        }
    };
    match (kind, value.kind) {
        (None | Some(ValueKind::Null), ScalarKind::Null) => Ok(SlotValue::RawSpan(value.span)),
        (None | Some(ValueKind::Boolean), ScalarKind::True) => Ok(SlotValue::Bool(true)),
        (None | Some(ValueKind::Boolean), ScalarKind::False) => Ok(SlotValue::Bool(false)),
        (None | Some(ValueKind::Integer | ValueKind::Number), ScalarKind::Number) => {
            let lexeme = number_grammar(token, numeric, budget, offset)?;
            match kind {
                Some(ValueKind::Integer) => {
                    if !lexeme.integral {
                        return Err(type_error(offset));
                    }
                    let converted = integer(token, lexeme.negative, offset)?;
                    integer_bounds(
                        converted,
                        &current.ok_or_else(|| internal(offset))?.constraints,
                        offset,
                    )?;
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
            if !lexeme.integral {
                return Err(type_error(offset));
            }
            let converted = integer(text.as_bytes(), lexeme.negative, offset)?;
            integer_bounds(
                converted,
                &current.ok_or_else(|| internal(offset))?.constraints,
                offset,
            )?;
            Ok(converted)
        }
        (Some(ValueKind::Decimal), ScalarKind::String) => {
            let constraints = current
                .and_then(|current| current.constraints.decimal.as_ref())
                .ok_or_else(|| internal(offset))?;
            decimal(
                text.ok_or_else(|| internal(offset))?,
                constraints,
                numeric,
                budget,
                offset,
            )
            .map(SlotValue::Decimal)
        }
        (None | Some(ValueKind::String), ScalarKind::String) => {
            let reference = value.decoded.ok_or_else(|| internal(offset))?;
            if let Some(current) = current {
                check_count(&current.constraints, reference.scalar_len, offset)?;
                let text = text.ok_or_else(|| internal(offset))?;
                if let Some(id) = current.constraints.pattern {
                    let index = usize::try_from(id).map_err(|_| internal(offset))?;
                    let pattern = program
                        .program()
                        .patterns
                        .get(index)
                        .ok_or_else(|| internal(offset))?;
                    check_pattern_validated(program, id, pattern, text, budget, offset)?;
                }
                if let Some(format) = current.constraints.format {
                    check_format(
                        format,
                        text,
                        &program.program().format_options,
                        budget,
                        offset,
                    )?;
                }
            }
            Ok(SlotValue::Str(reference))
        }
        _ => Err(type_error(offset)),
    }
}

/// Apply request numeric policy as well as the compiled schema policy.
/// The cursor validates syntax; this uses the shared VM work budget for the
/// additional exponent, negative-zero and representation checks.
pub(crate) fn check_input_number(
    bytes: &[u8],
    policy: &NumericPolicy,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    number_grammar(bytes, policy, budget, offset)?;
    float(bytes, policy, offset)?;
    Ok(())
}

/// Check inclusive Unicode-scalar, array-item, or object-member bounds.
pub fn check_count(constraints: &ConstraintSet, count: u32, offset: u32) -> Result<()> {
    if constraints.min_count.is_some_and(|minimum| count < minimum)
        || constraints.max_count.is_some_and(|maximum| count > maximum)
    {
        return Err(vm_error(
            ErrorCode::ConstraintViolation,
            offset,
            "count is outside inclusive schema bounds",
        ));
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
    let scratch = states
        .checked_mul(128)
        .and_then(|size| size.checked_add(256))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| resource(offset))?;
    // Raw PatternPrograms are accepted here, so pay for structural validation
    // and class-range scanning as well as the matcher's own transition charges.
    let setup = states
        .checked_mul(32)
        .and_then(|units| units.checked_add(bytes))
        .and_then(|units| units.checked_add(1))
        .ok_or_else(|| resource(offset))?;
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
        false => Err(vm_error(
            ErrorCode::PatternMismatch,
            offset,
            "string does not match the compiled pattern",
        )),
    }
}

fn check_pattern_validated(
    program: &ValidatedProgram,
    id: u32,
    pattern: &PatternProgram,
    text: &str,
    budget: &mut VmBudget,
    offset: u32,
) -> Result<()> {
    let states = pattern.ops.len() as u64;
    let scratch = states
        .checked_mul(128)
        .and_then(|size| size.checked_add(256))
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| resource(offset))?;
    budget.retain(scratch, offset)?;
    let result = jmax_schema_compile::patterns::is_match_validated(
        program,
        id,
        text,
        &mut budget.steps_left,
    );
    budget.release(scratch)?;
    if result.map_err(|error| at_token(error, offset))? {
        Ok(())
    } else {
        Err(vm_error(
            ErrorCode::PatternMismatch,
            offset,
            "string does not match the compiled pattern",
        ))
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
    if u64::try_from(text.len()).map_err(|_| resource(offset))? > u64::from(options.max_input_bytes)
    {
        return Err(
            jmax_format::FormatFailure::new(jmax_format::FormatFailureKind::Limit, 0)
                .into_error(offset, ErrorSource::SchemaVm),
        );
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
    if constraints
        .min_i64
        .is_some_and(|minimum| value < i128::from(minimum))
        || constraints
            .max_i64
            .is_some_and(|maximum| value > i128::from(maximum))
    {
        return Err(vm_error(
            ErrorCode::ConstraintViolation,
            offset,
            "integer is outside inclusive schema bounds",
        ));
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
    if policy.max_exponent_abs < 0 {
        return Err(internal(offset));
    }
    let negative = bytes.first() == Some(&b'-');
    let mut cursor = usize::from(negative);
    let start = cursor;
    match bytes.get(cursor) {
        Some(b'0') => cursor += 1,
        Some(b'1'..=b'9') => {
            cursor += 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
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
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == start {
            return Err(invalid_number(offset));
        }
        fraction = bytes.get(start..cursor).ok_or_else(|| internal(offset))?;
    }
    let has_exponent = matches!(bytes.get(cursor), Some(b'e' | b'E'));
    let mut exponent = 0i32;
    if has_exponent {
        integral = false;
        cursor += 1;
        let negative_exponent = bytes.get(cursor) == Some(&b'-');
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        let start = cursor;
        while let Some(digit @ b'0'..=b'9') = bytes.get(cursor).copied() {
            exponent = exponent
                .checked_mul(10)
                .and_then(|value| value.checked_add(i32::from(digit - b'0')))
                .filter(|value| *value <= i32::from(policy.max_exponent_abs))
                .ok_or_else(|| invalid_number(offset))?;
            cursor += 1;
        }
        if cursor == start {
            return Err(invalid_number(offset));
        }
        if negative_exponent {
            exponent = -exponent;
        }
    }
    if cursor != bytes.len() {
        return Err(invalid_number(offset));
    }
    if policy.reject_negative_zero
        && negative
        && integer.iter().chain(fraction).all(|digit| *digit == b'0')
    {
        return Err(vm_error(
            ErrorCode::InvalidNumber,
            offset,
            "negative zero is forbidden",
        ));
    }
    Ok(NumberGrammar {
        negative,
        integral,
        integer,
        fraction,
        exponent,
        has_exponent,
    })
}

/// Called only after the complete integer grammar has been checked.
fn integer(bytes: &[u8], negative: bool, offset: u32) -> Result<SlotValue> {
    if negative {
        let mut value = 0i64;
        for &digit in bytes.get(1..).ok_or_else(|| internal(offset))? {
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_sub(i64::from(digit - b'0')))
                .ok_or_else(|| overflow(offset))?;
        }
        Ok(SlotValue::I64(value))
    } else {
        let mut value = 0u64;
        for &digit in bytes {
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(digit - b'0')))
                .ok_or_else(|| overflow(offset))?;
        }
        Ok(SlotValue::U64(value))
    }
}

fn float(bytes: &[u8], policy: &NumericPolicy, offset: u32) -> Result<SlotValue> {
    let text = core::str::from_utf8(bytes).map_err(|_| invalid_number(offset))?;
    let value = text.parse::<f64>().map_err(|_| overflow(offset))?;
    if policy.reject_non_finite_float && !value.is_finite() {
        return Err(overflow(offset));
    }
    // Also reject a negative nonzero lexeme that underflows to negative zero.
    if policy.reject_negative_zero && value == 0.0 && value.is_sign_negative() {
        return Err(vm_error(
            ErrorCode::InvalidNumber,
            offset,
            "negative zero is forbidden",
        ));
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
    if lexeme.has_exponent && !policy.allow_exponent {
        return Err(invalid_number(offset));
    }
    // Accumulate decoded digits directly, so JSON escapes need no temporary
    // quoted allocation. Signed accumulation includes i128::MIN exactly.
    let mut coefficient = 0i128;
    for &digit in lexeme.integer.iter().chain(lexeme.fraction) {
        let digit = i128::from(digit - b'0');
        let digit = if lexeme.negative { -digit } else { digit };
        coefficient = coefficient
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
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
    jmax_decimal::bounds::validate(
        value,
        policy,
        jmax_decimal::DecimalBounds {
            min: constraints.minimum,
            max: constraints.maximum,
        },
        offset,
    )
    .map_err(|error| at_token(error, offset))?;
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
        return Err(vm_error(
            ErrorCode::InvalidSyntax,
            offset,
            "string token lacks quotes",
        ));
    }
    if reference.source == SpanSource::RawPayload {
        let start = span.offset.checked_add(1).ok_or_else(|| internal(offset))?;
        let len = span.len.checked_sub(2).ok_or_else(|| internal(offset))?;
        if reference.span != Span32::new(start, len) {
            return Err(internal(offset));
        }
    }
    let interior = token
        .get(1..token.len() - 1)
        .ok_or_else(|| internal(offset))?;
    let interior = core::str::from_utf8(interior)
        .map_err(|_| vm_error(ErrorCode::InvalidUtf8, offset, "raw string is not UTF-8"))?;
    let mut raw = interior.chars();
    let mut expected = decoded.chars();
    while let Some(scalar) = raw.next() {
        let scalar = match scalar {
            '"' => {
                return Err(vm_error(
                    ErrorCode::InvalidSyntax,
                    offset,
                    "unescaped quote in string",
                ));
            }
            '\u{0}'..='\u{1f}' => {
                return Err(vm_error(
                    ErrorCode::ControlCharInString,
                    offset,
                    "unescaped control character in string",
                ));
            }
            '\\' => {
                if reference.source == SpanSource::RawPayload {
                    return Err(internal(offset));
                }
                match raw.next() {
                    Some('"') => '"',
                    Some('\\') => '\\',
                    Some('/') => '/',
                    Some('b') => '\u{8}',
                    Some('f') => '\u{c}',
                    Some('n') => '\n',
                    Some('r') => '\r',
                    Some('t') => '\t',
                    Some('u') => {
                        let first = hex_quad(&mut raw, offset)?;
                        let scalar = match first {
                            0xd800..=0xdbff => {
                                if raw.next() != Some('\\') || raw.next() != Some('u') {
                                    return Err(lone_surrogate(offset));
                                }
                                let second = hex_quad(&mut raw, offset)?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(lone_surrogate(offset));
                                }
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            }
                            0xdc00..=0xdfff => return Err(lone_surrogate(offset)),
                            _ => first,
                        };
                        char::from_u32(scalar).ok_or_else(|| lone_surrogate(offset))?
                    }
                    _ => {
                        return Err(vm_error(
                            ErrorCode::InvalidEscape,
                            offset,
                            "invalid JSON escape",
                        ));
                    }
                }
            }
            scalar => scalar,
        };
        if expected.next() != Some(scalar) {
            return Err(internal(offset));
        }
    }
    if expected.next().is_some() {
        return Err(internal(offset));
    }
    Ok(())
}

fn hex_quad(chars: &mut core::str::Chars<'_>, offset: u32) -> Result<u32> {
    let mut value = 0u32;
    for _ in 0..4 {
        let digit = chars
            .next()
            .and_then(|scalar| scalar.to_digit(16))
            .ok_or_else(|| vm_error(ErrorCode::InvalidEscape, offset, "invalid Unicode escape"))?;
        // Exactly four hexadecimal digits, so this cannot exceed u16::MAX.
        value = value * 16 + digit;
    }
    Ok(value)
}

fn charge_bytes(budget: &mut VmBudget, len: usize, passes: u64, offset: u32) -> Result<()> {
    let units = u64::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(passes))
        .ok_or_else(|| resource(offset))?;
    budget.charge(units, offset)
}

fn at_token(error: Error, offset: u32) -> Error {
    Error {
        byte_offset: offset,
        ..error
    }
}
fn internal(offset: u32) -> Error {
    vm_error(
        ErrorCode::InternalError,
        offset,
        "inconsistent scalar metadata or validated program",
    )
}
fn resource(offset: u32) -> Error {
    vm_error(
        ErrorCode::BudgetExhausted,
        offset,
        "scalar constraint resource limit exceeded",
    )
}
fn invalid_number(offset: u32) -> Error {
    vm_error(
        ErrorCode::InvalidNumber,
        offset,
        "invalid number grammar or exponent policy",
    )
}
fn overflow(offset: u32) -> Error {
    vm_error(
        ErrorCode::NumberOverflow,
        offset,
        "number is outside its representation range",
    )
}
fn type_error(offset: u32) -> Error {
    vm_error(
        ErrorCode::TypeMismatch,
        offset,
        "scalar does not match the schema kind",
    )
}
fn lone_surrogate(offset: u32) -> Error {
    vm_error(
        ErrorCode::LoneSurrogate,
        offset,
        "Unicode escape contains an unpaired surrogate",
    )
}
