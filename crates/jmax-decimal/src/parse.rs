//! Checked, allocation-free accumulation and quoted-decimal conversion.

use super::*;

/// Concatenate the integer and fractional digits into a bounded coefficient.
///
/// The lexer's checked span proves that digit lengths and absolute offsets fit
/// in `u32`. Leading zeros do not contribute to precision, but every fractional
/// digit contributes to the source scale. The exponent is preserved unchanged.
/// Coefficient overflow is reported at the first unrepresentable digit, before
/// any rescaling can discard digits.
pub fn accumulate(lexeme: &DecimalLexeme<'_>) -> Result<AccumulatedDecimal> {
    // Quotes and an optional minus precede the integer. The point, when
    // present, occupies exactly one byte between the two digit slices.
    let integer_offset = lexeme.span().offset + 1 + u32::from(lexeme.is_negative());
    let fraction_offset = integer_offset + lexeme.integer_digits().len() as u32 + 1;
    let coefficient = accumulate_digits(
        0,
        lexeme.integer_digits(),
        lexeme.is_negative(),
        integer_offset,
    )?;
    let coefficient = accumulate_digits(
        coefficient,
        lexeme.fractional_digits(),
        lexeme.is_negative(),
        fraction_offset,
    )?;

    Ok(AccumulatedDecimal {
        coefficient,
        fractional_digits: lexeme.fractional_digits().len() as u32,
        exponent: lexeme.exponent(),
        precision: coefficient_precision(coefficient),
    })
}

/// Parse one complete quoted token under the supplied decimal policy.
///
/// Configuration is validated before inspecting the input. Lexing,
/// accumulation, rescaling, and final validation preserve their original error
/// metadata. The returned representation has exactly `policy.target_scale`.
pub fn parse_quoted(
    input: &[u8],
    offset: u32,
    policy: &DecimalPolicy,
) -> Result<Decimal128> {
    let _t = jmax_trace::span!(DECIMAL_PARSE);
    validate_policy(policy)?;
    let lexeme = lex::lex_quoted(input, offset, policy.allow_exponent)?;
    let accumulated = accumulate(&lexeme)?;
    let value = bounds::rescale(accumulated, policy, offset)?;
    bounds::validate(value, policy, DecimalBounds::default(), offset)?;
    Ok(value)
}

// Each slice contains only ASCII digits, as established by the lexer. Applying
// the lexical sign at every step avoids ever constructing the positive signed
// magnitude of i128::MIN. Chunks restart at the point; the accumulated
// coefficient does not.
fn accumulate_digits(
    mut coefficient: i128,
    digits: &[u8],
    negative: bool,
    mut offset: u32,
) -> Result<i128> {
    for chunk in digits.chunks(CHUNK_DIGITS as usize) {
        let mut magnitude = 0_u32;
        let mut base = 1_u32;
        for &digit in chunk {
            // At most nine digits: magnitude <= CHUNK_BASE - 1 and
            // base <= CHUNK_BASE. Both intermediates fit in u32.
            magnitude = magnitude * 10 + u32::from(digit - b'0');
            base *= 10;
        }
        let signed_chunk = if negative {
            -i128::from(magnitude)
        } else {
            i128::from(magnitude)
        };
        if let Some(next) = coefficient
            .checked_mul(i128::from(base))
            .and_then(|value| value.checked_add(signed_chunk))
        {
            coefficient = next;
        } else {
            // Prefix magnitudes never decrease. Replay only a failing chunk
            // to report the earliest digit that exceeds the signed range.
            for (index, &digit) in chunk.iter().enumerate() {
                let digit = i128::from(digit - b'0');
                let signed_digit = if negative { -digit } else { digit };
                coefficient = coefficient
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(signed_digit))
                    .ok_or_else(|| {
                        decimal_error(
                            ErrorCode::NumberOverflow,
                            offset + index as u32,
                            "decimal coefficient exceeds i128 range",
                        )
                    })?;
            }
        }
        // The lexer's validated absolute end bounds this addition and every
        // narrowing conversion above, including the end of the final chunk.
        offset += chunk.len() as u32;
    }
    Ok(coefficient)
}
