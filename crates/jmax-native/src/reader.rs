//! Token primitives. Each skips whitespace, consumes exactly one token on
//! success and leaves the position unchanged on `Fail`.

use crate::{Decimal128, DecimalPolicy, Fail, NativeContext, NativeResult};
use jmax_number::NumberToken;
use jmax_abi::span::NumberLexKind;

#[derive(Clone, Debug)]
pub struct Reader<'a> {
    input: &'a [u8],
    pos: usize,
}

/// Bytes that may follow a literal or number token.
#[inline(always)]
const fn is_delim(byte: Option<u8>) -> bool {
    matches!(byte, None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'))
}

impl<'a> Reader<'a> {
    pub const fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    pub const fn input(&self) -> &'a [u8] {
        self.input
    }

    pub const fn pos(&self) -> usize {
        self.pos
    }

    #[inline(always)]
    pub fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\r' | b'\n') = self.input.get(self.pos) {
            self.pos += 1;
        }
    }

    /// The next non-whitespace byte, not consumed.
    #[inline(always)]
    pub fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.input.get(self.pos).copied()
    }

    #[inline(always)]
    pub fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn expect(&mut self, byte: u8) -> NativeResult<()> {
        if self.eat(byte) { Ok(()) } else { Err(Fail) }
    }

    /// Only whitespace remains.
    pub fn end(&mut self) -> NativeResult<()> {
        self.skip_ws();
        if self.pos == self.input.len() { Ok(()) } else { Err(Fail) }
    }

    pub fn boolean(&mut self) -> NativeResult<bool> {
        self.skip_ws();
        let rest = &self.input[self.pos..];
        if rest.starts_with(b"true") && is_delim(rest.get(4).copied()) {
            self.pos += 4;
            Ok(true)
        } else if rest.starts_with(b"false") && is_delim(rest.get(5).copied()) {
            self.pos += 5;
            Ok(false)
        } else {
            Err(Fail)
        }
    }

    /// A nonnegative integer no greater than `max`.
    pub fn uint(&mut self, ctx: &mut NativeContext, max: u64) -> NativeResult<u64> {
        self.skip_ws();
        let input = self.input;
        let start = self.pos;
        let digits = ctx.limits.syntax.max_number_digits;
        if let Some(b'0'..=b'9') = input.get(start) {
            let mut value = 0u64;
            let mut i = start;
            while let Some(&byte @ b'0'..=b'9') = input.get(i) {
                if i - start == 19 {
                    return self.uint_slow(ctx, max);
                }
                value = value * 10 + u64::from(byte - b'0');
                i += 1;
            }
            let count = i - start;
            // A leading zero is only valid as the whole integer part.
            if is_delim(input.get(i).copied())
                && (input[start] != b'0' || count == 1)
                && count <= usize::from(digits)
            {
                if value > max {
                    return Err(Fail);
                }
                self.pos = i;
                return Ok(value);
            }
        }
        self.uint_slow(ctx, max)
    }

    fn uint_slow(&mut self, ctx: &mut NativeContext, max: u64) -> NativeResult<u64> {
        let value = self.integer_slow(ctx, 0, i128::from(max))?;
        u64::try_from(value).map_err(|_| Fail)
    }

    /// An integer within `min..=max`.
    pub fn int(&mut self, ctx: &mut NativeContext, min: i64, max: i64) -> NativeResult<i64> {
        self.skip_ws();
        let input = self.input;
        let start = self.pos;
        let digits = ctx.limits.syntax.max_number_digits;
        if input.get(start) == Some(&b'-') {
            if let Some(b'1'..=b'9') = input.get(start + 1) {
                let mut value = 0u64;
                let mut i = start + 1;
                while let Some(&byte @ b'0'..=b'9') = input.get(i) {
                    if i - start == 19 {
                        return self.int_slow(ctx, min, max);
                    }
                    value = value * 10 + u64::from(byte - b'0');
                    i += 1;
                }
                if is_delim(input.get(i).copied()) && i - start - 1 <= usize::from(digits) {
                    let value = -i128::from(value);
                    if value < i128::from(min) || value > i128::from(max) {
                        return Err(Fail);
                    }
                    self.pos = i;
                    return Ok(value as i64);
                }
            }
            return self.int_slow(ctx, min, max);
        }
        match self.uint(ctx, u64::MAX.min(i64::MAX as u64)) {
            Ok(value) => {
                let value = value as i64;
                if value < min || value > max { Err(Fail) } else { Ok(value) }
            }
            // uint may have consumed nothing; the slow path decides.
            Err(Fail) => self.int_slow(ctx, min, max),
        }
    }

    fn int_slow(&mut self, ctx: &mut NativeContext, min: i64, max: i64) -> NativeResult<i64> {
        let value = self.integer_slow(ctx, i128::from(min), i128::from(max))?;
        i64::try_from(value).map_err(|_| Fail)
    }

    /// Lex an integer with jmax-number (and decode a quoted one the way the VM
    /// does), checking `min..=max`. Consumes the token only on success.
    fn integer_slow(&mut self, ctx: &mut NativeContext, min: i128, max: i128) -> NativeResult<i128> {
        let start = self.pos;
        let digits = u32::from(ctx.limits.syntax.max_number_digits);
        let (value, end) = if self.input.get(start) == Some(&b'"') {
            if !ctx.numeric.allow_Quoted_int {
                return Err(Fail);
            }
            let schema = ctx.schema_numeric;
            let text = self.string_at(ctx, start)?;
            let (text, end) = text;
            let bytes = text.as_bytes();
            // The number sits inside a string: the program's own policy applies.
            if bytes.first() == Some(&b'"') {
                return Err(Fail);
            }
            let token = jmax_number::lex::lex_number(bytes, 0, &schema).map_err(|_| Fail)?;
            if token.span().len as usize != bytes.len() {
                return Err(Fail);
            }
            (convert(&token, digits)?, end)
        } else {
            let offset = u32::try_from(start).map_err(|_| Fail)?;
            let token = jmax_number::lex::lex_number(self.input, offset, &ctx.numeric).map_err(|_| Fail)?;
            let end = start + token.span().len as usize;
            (convert(&token, digits)?, end)
        };
        if value < min || value > max {
            return Err(Fail);
        }
        self.pos = end;
        Ok(value)
    }

    /// A JSON number as binary64 under the effective numeric policy.
    pub fn float(&mut self, ctx: &mut NativeContext) -> NativeResult<f64> {
        self.skip_ws();
        let start = self.pos;
        let offset = u32::try_from(start).map_err(|_| Fail)?;
        let token = jmax_number::lex::lex_number(self.input, offset, &ctx.numeric).map_err(|_| Fail)?;
        if token.is_quoted() {
            return Err(Fail);
        }
        let digits = token.raw_bytes().iter().filter(|byte| byte.is_ascii_digit()).count();
        if digits > usize::from(ctx.limits.syntax.max_number_digits) {
            return Err(Fail);
        }
        let value = jmax_number::float::parse_f64(&token, &ctx.numeric).map_err(|_| Fail)?;
        self.pos = start + token.span().len as usize;
        Ok(value)
    }

    /// A JSON string holding a fixed-point decimal, as `jmax-decimal` computes it.
    pub fn decimal(&mut self, ctx: &mut NativeContext, policy: &DecimalPolicy) -> NativeResult<Decimal128> {
        self.skip_ws();
        let start = self.pos;
        let numeric = ctx.schema_numeric;
        let digits_limit = usize::from(ctx.limits.syntax.max_number_digits);
        let (text, end) = self.string_at(ctx, start)?;
        let value = parse_decimal(text.as_bytes(), policy, &numeric, digits_limit)?;
        self.pos = end;
        Ok(value)
    }

    /// A string. Without escapes it borrows the input; with escapes it is
    /// decoded once into the context's buffer.
    pub fn string<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c str>
    where
        'a: 'c,
    {
        self.skip_ws();
        let (text, end) = self.string_at(ctx, self.pos)?;
        self.pos = end;
        Ok(text)
    }

    /// An object key and its colon.
    pub fn key<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c str>
    where
        'a: 'c,
    {
        let key = self.string(ctx)?;
        self.expect(b':')?;
        Ok(key)
    }

    /// The string starting at `start` and the offset just past it; no state changes.
    fn string_at<'c>(&self, ctx: &'c mut NativeContext, start: usize) -> NativeResult<(&'c str, usize)>
    where
        'a: 'c,
    {
        let input: &'a [u8] = self.input;
        if input.get(start) != Some(&b'"') {
            return Err(Fail);
        }
        let max = ctx.limits.syntax.max_string_bytes as usize;
        let content_start = start + 1;
        let special = jmax_string::scan::first_special(&input[content_start..]).ok_or(Fail)?;
        let at = content_start + special;
        match input[at] {
            b'"' => {
                let content = &input[content_start..at];
                if content.len() > max {
                    return Err(Fail);
                }
                Ok((core::str::from_utf8(content).map_err(|_| Fail)?, at + 1))
            }
            b'\\' => {
                let opening = u32::try_from(start).map_err(|_| Fail)?;
                let scan = jmax_string::scan::scan_string(input, opening).map_err(|_| Fail)?;
                if scan.content.len as usize > max {
                    return Err(Fail);
                }
                ctx.arena.clear();
                let lengths = jmax_string::unescape::unescape(input, scan.content, &mut ctx.arena)
                    .map_err(|_| Fail)?;
                if lengths.byte_len as usize > max {
                    return Err(Fail);
                }
                let text = core::str::from_utf8(&ctx.arena).map_err(|_| Fail)?;
                Ok((text, scan.next_offset as usize))
            }
            _ => Err(Fail),
        }
    }

    /// Strictly validate and discard one value; `depth` counts enclosing containers.
    pub fn skip_value(&mut self, ctx: &mut NativeContext, depth: u16) -> NativeResult<()> {
        self.skip_ws();
        let start = u32::try_from(self.pos).map_err(|_| Fail)?;
        let owner = if ctx.limits.syntax.reject_duplicate_keys {
            jmax_abi::syntax::DuplicatePolicyOwner::SchemaVm
        } else {
            jmax_abi::syntax::DuplicatePolicyOwner::Disabled
        };
        ctx.skip_decoded.clear();
        let end = jmax_syntax::skip_strict(
            self.input,
            start,
            depth,
            ctx.limits.syntax,
            Some(ctx.numeric),
            owner,
            &mut ctx.skip_decoded,
            &mut ctx.skip,
        )
        .map_err(|_| Fail)?;
        self.pos = end as usize;
        Ok(())
    }
}

fn convert(token: &NumberToken<'_>, digits_limit: u32) -> NativeResult<i128> {
    if !matches!(token.kind(), NumberLexKind::UnsignedInt | NumberLexKind::SignedInt) {
        return Err(Fail);
    }
    if token.integer_digits() > digits_limit {
        return Err(Fail);
    }
    match jmax_number::integer::convert_integer(token).map_err(|_| Fail)? {
        jmax_abi::number::ConvertedNumber::U64(value) => Ok(i128::from(value)),
        jmax_abi::number::ConvertedNumber::I64(value) => Ok(i128::from(value)),
        _ => Err(Fail),
    }
}

/// The VM's decimal path: JSON number grammar under the program's numeric
/// policy, exact coefficient accumulation, then jmax-decimal rescale/validate.
fn parse_decimal(
    bytes: &[u8],
    policy: &DecimalPolicy,
    numeric: &jmax_abi::number::NumericPolicy,
    digits_limit: usize,
) -> NativeResult<Decimal128> {
    jmax_decimal::validate_policy(policy).map_err(|_| Fail)?;
    if bytes.iter().filter(|byte| byte.is_ascii_digit()).count() > digits_limit {
        return Err(Fail);
    }
    let negative = bytes.first() == Some(&b'-');
    let mut i = usize::from(negative);
    let int_start = i;
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return Err(Fail),
    }
    let integer = &bytes[int_start..i];
    let mut fraction: &[u8] = &[];
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == frac_start {
            return Err(Fail);
        }
        fraction = &bytes[frac_start..i];
    }
    let mut exponent = 0i32;
    let has_exponent = matches!(bytes.get(i), Some(b'e' | b'E'));
    if has_exponent {
        i += 1;
        let negative_exponent = bytes.get(i) == Some(&b'-');
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_start = i;
        while let Some(&digit @ b'0'..=b'9') = bytes.get(i) {
            exponent = exponent
                .checked_mul(10)
                .and_then(|value| value.checked_add(i32::from(digit - b'0')))
                .filter(|value| *value <= i32::from(numeric.max_exponent_abs))
                .ok_or(Fail)?;
            i += 1;
        }
        if i == exp_start {
            return Err(Fail);
        }
        if negative_exponent {
            exponent = -exponent;
        }
    }
    if i != bytes.len() {
        return Err(Fail);
    }
    if numeric.reject_negative_zero
        && negative
        && integer.iter().chain(fraction).all(|digit| *digit == b'0')
    {
        return Err(Fail);
    }
    if has_exponent && !policy.allow_exponent {
        return Err(Fail);
    }
    let mut coefficient = 0i128;
    for &digit in integer.iter().chain(fraction) {
        let digit = i128::from(digit - b'0');
        let digit = if negative { -digit } else { digit };
        coefficient = coefficient
            .checked_mul(10)
            .and_then(|value| value.checked_add(digit))
            .ok_or(Fail)?;
    }
    let accumulated = jmax_decimal::AccumulatedDecimal {
        coefficient,
        fractional_digits: u32::try_from(fraction.len()).map_err(|_| Fail)?,
        exponent,
        precision: jmax_decimal::coefficient_precision(coefficient),
    };
    let value = jmax_decimal::bounds::rescale(accumulated, policy, 0).map_err(|_| Fail)?;
    jmax_decimal::bounds::validate(value, policy, jmax_decimal::DecimalBounds::default(), 0)
        .map_err(|_| Fail)?;
    Ok(value)
}
