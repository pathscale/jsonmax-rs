//! Exact, allocation-free integer conversion with checked eight-digit batches.

use super::*;

/// Convert an integer token to an unsigned 64-bit value.
///
/// Every negative token, including an allowed lexical `-0`, overflows. Decimal
/// and exponent spellings are rejected even when their value is integral.
/// Quoted integers retain the policy already established by the lexer.
/// Errors use the token's full source-span start and [`ErrorSource::Lexer`].
pub fn parse_u64(token: &NumberToken<'_>) -> Result<u64> {
    require_integer(token)?;
    if token.is_negative() {
        return Err(overflow(token));
    }
    magnitude(token, u64::MAX)
}

/// Convert an integer token to a signed 64-bit value, including `i64::MIN`.
///
/// Negative zero becomes zero if the lexer admitted it. Float spellings are
/// rejected, and every out-of-range integer reports [`ErrorCode::NumberOverflow`]
/// at the token's full source-span start with [`ErrorSource::Lexer`].
pub fn parse_i64(token: &NumberToken<'_>) -> Result<i64> {
    require_integer(token)?;
    let negative = token.is_negative();
    const MIN_MAGNITUDE: u64 = 1_u64 << 63;
    let limit = if negative { MIN_MAGNITUDE } else { i64::MAX as u64 };
    let value = magnitude(token, limit)?;
    if negative && value == MIN_MAGNITUDE {
        return Ok(i64::MIN);
    }
    let value = i64::try_from(value).map_err(|_| overflow(token))?;
    // The minimum magnitude was handled above. The remaining value lies in
    // 0..=i64::MAX, so its signed negation is representable, including zero.
    Ok(if negative { -value } else { value })
}

/// Convert by lexical sign: negative integers become `I64`, others become `U64`.
///
/// Wrapper quotes do not affect the choice. Range failures never fall back to
/// floating-point conversion, and no new lexical policy is imposed.
pub fn convert_integer(token: &NumberToken<'_>) -> Result<ConvertedNumber> {
    if token.is_negative() {
        parse_i64(token).map(ConvertedNumber::I64)
    } else {
        parse_u64(token).map(ConvertedNumber::U64)
    }
}

fn require_integer(token: &NumberToken<'_>) -> Result<()> {
    match token.kind() {
        NumberLexKind::UnsignedInt | NumberLexKind::SignedInt | NumberLexKind::QuotedDecimal => {
            Ok(())
        }
        NumberLexKind::FloatDecimal => Err(Error::new(
            ErrorCode::InvalidNumber,
            ErrorSource::Lexer,
            token.span().offset,
            "integer conversion requires an integer spelling",
        )),
    }
}

fn overflow(token: &NumberToken<'_>) -> Error {
    Error::new(
        ErrorCode::NumberOverflow,
        ErrorSource::Lexer,
        token.span().offset,
        "integer is outside the destination range",
    )
}

// Called only after require_integer. Lex constructs all tokens, so the raw
// spelling is a nonempty ASCII digit sequence with an optional leading minus;
// wrapper quotes are already excluded. No grammar or policy is reinterpreted.
fn magnitude(token: &NumberToken<'_>, limit: u64) -> Result<u64> {
    let raw = token.raw_bytes();
    let digits = if token.is_negative() { &raw[1..] } else { raw };
    let mut chunks = digits.chunks_exact(8);
    let mut value = 0_u64;
    for chunk in &mut chunks {
        // chunks_exact establishes the bound before copying. The explicit
        // little-endian load neither requires alignment nor depends on the host.
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(chunk);
        value = value
            .checked_mul(100_000_000)
            .and_then(|prefix| prefix.checked_add(eight_digits(bytes)))
            .filter(|&next| next <= limit)
            .ok_or_else(|| overflow(token))?;
    }
    for &byte in chunks.remainder() {
        value = value
            .checked_mul(10)
            .and_then(|prefix| prefix.checked_add(u64::from(byte - b'0')))
            .filter(|&next| next <= limit)
            .ok_or_else(|| overflow(token))?;
    }
    Ok(value)
}

// Reduce eight known ASCII digits in parallel, without wrapping arithmetic.
// Subtraction cannot borrow between byte lanes because each byte is >= '0'.
// Pair lanes are <= 99 in 16 bits; quad lanes are <= 9999 in 32 bits. The
// masks leave enough space for each multiplication and addition, including in
// the highest lane. The final scalar value is <= 99_999_999 and fits in u64.
fn eight_digits(bytes: [u8; 8]) -> u64 {
    let digits = u64::from_le_bytes(bytes) - 0x3030_3030_3030_3030;
    const BYTE_LANES: u64 = 0x00ff_00ff_00ff_00ff;
    const PAIR_LANES: u64 = 0x0000_ffff_0000_ffff;
    let pairs = (digits & BYTE_LANES) * 10 + ((digits >> 8) & BYTE_LANES);
    let quads = (pairs & PAIR_LANES) * 100 + ((pairs >> 16) & PAIR_LANES);
    (quads & 0xffff_ffff) * 10_000 + (quads >> 32)
}
