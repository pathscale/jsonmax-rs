//! Token primitives. Each skips whitespace, consumes exactly one token on
//! success and leaves the position unchanged on `Fail`.

use crate::{Decimal128, DecimalPolicy, Fail, FormatId, NativeContext, NativeResult};
use alloc::vec::Vec;
use jmax_abi::span::NumberLexKind;
use jmax_number::NumberToken;

/// Decode text that `FormatId::Base64` has accepted: the standard alphabet, padded
/// to a multiple of four, canonical trailing bits.
fn base64_decode(text: &[u8], out: &mut Vec<u8>) {
    fn value(byte: u8) -> u32 {
        match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            _ => 63,
        }
    }
    out.reserve(text.len() / 4 * 3);
    for quantum in text.chunks(4) {
        let data = quantum.iter().take_while(|byte| **byte != b'=').count();
        let mut bits = 0u32;
        for (index, &byte) in quantum.iter().enumerate() {
            bits |= (if index < data { value(byte) } else { 0 }) << (18 - 6 * index);
        }
        let bytes = bits.to_be_bytes();
        out.extend_from_slice(&bytes[1..data.saturating_sub(1) + 1]);
    }
}

/// A scanned string: ASCII bytes (valid UTF-8 by construction) or validated text.
enum Scanned<'c> {
    Ascii(&'c [u8]),
    Text(&'c str),
}

#[derive(Clone, Debug)]
pub struct Reader<'a> {
    input: &'a [u8],
    pos: usize,
    /// Set by `decode_into` after it reset the context's skip memo for this input, so
    /// skipped values may share remembered key shapes. A hand-built reader never sets it.
    shared_skip: bool,
}

/// Bytes that may follow a literal or number token.
#[inline(always)]
const fn is_delim(byte: Option<u8>) -> bool {
    matches!(
        byte,
        None | Some(b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}')
    )
}

impl<'a> Reader<'a> {
    pub const fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            shared_skip: false,
        }
    }

    /// A reader over `input` whose context's skip memo was just reset for it.
    pub(crate) const fn new_shared_skip(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            shared_skip: true,
        }
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
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(Fail)
        }
    }

    #[inline]
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
    #[inline]
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
    #[inline]
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
                if value < min || value > max {
                    Err(Fail)
                } else {
                    Ok(value)
                }
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
    fn integer_slow(
        &mut self,
        ctx: &mut NativeContext,
        min: i128,
        max: i128,
    ) -> NativeResult<i128> {
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
            let token =
                jmax_number::lex::lex_number(self.input, offset, &ctx.numeric).map_err(|_| Fail)?;
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
        let token =
            jmax_number::lex::lex_number(self.input, offset, &ctx.numeric).map_err(|_| Fail)?;
        if token.is_quoted() {
            return Err(Fail);
        }
        let digits = token
            .raw_bytes()
            .iter()
            .filter(|byte| byte.is_ascii_digit())
            .count();
        if digits > usize::from(ctx.limits.syntax.max_number_digits) {
            return Err(Fail);
        }
        let value = jmax_number::float::parse_f64(&token, &ctx.numeric).map_err(|_| Fail)?;
        self.pos = start + token.span().len as usize;
        Ok(value)
    }

    /// A JSON string holding a fixed-point decimal, as `jmax-decimal` computes it.
    ///
    /// Canonical short decimals take an inline path (`fast_decimal`) that never
    /// answers differently from the general one; everything else, including every
    /// rejection, goes to the general path.
    #[inline]
    pub fn decimal(
        &mut self,
        ctx: &mut NativeContext,
        policy: &DecimalPolicy,
    ) -> NativeResult<Decimal128> {
        self.skip_ws();
        let digits_limit = usize::from(ctx.limits.syntax.max_number_digits);
        let max_string = ctx.limits.syntax.max_string_bytes as usize;
        if let Some((value, end)) =
            fast_decimal(self.input, self.pos, policy, digits_limit, max_string)
        {
            self.pos = end;
            // Saturation remains an upper bound; the checked budget multiply
            // then declines the shortcut, without a branch per decimal.
            ctx.decimals = ctx.decimals.saturating_add(1);
            return Ok(value);
        }
        self.decimal_slow(ctx, policy)
    }

    #[cold]
    #[inline(never)]
    fn decimal_slow(
        &mut self,
        ctx: &mut NativeContext,
        policy: &DecimalPolicy,
    ) -> NativeResult<Decimal128> {
        self.skip_ws();
        let start = self.pos;
        let numeric = ctx.schema_numeric;
        let digits_limit = usize::from(ctx.limits.syntax.max_number_digits);
        let (text, end) = self.string_at(ctx, start)?;
        let value = parse_decimal(text.as_bytes(), policy, &numeric, digits_limit)?;
        self.pos = end;
        ctx.decimals = ctx.decimals.saturating_add(1);
        Ok(value)
    }

    /// Consume a JSON `null` literal, if one is next; otherwise nothing but whitespace.
    #[inline]
    pub fn null(&mut self) -> bool {
        self.skip_ws();
        let rest = &self.input[self.pos..];
        if rest.starts_with(b"null") && is_delim(rest.get(4).copied()) {
            self.pos += 4;
            true
        } else {
            false
        }
    }

    /// Consume an empty JSON object, preserving the position on failure.
    #[inline]
    pub fn empty_object(&mut self) -> NativeResult<()> {
        let start = self.pos;
        self.skip_ws();
        if self.input.get(self.pos) != Some(&b'{') {
            self.pos = start;
            return Err(Fail);
        }
        self.pos += 1;
        self.skip_ws();
        if self.input.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            Ok(())
        } else {
            self.pos = start;
            Err(Fail)
        }
    }

    /// A string that must pass the named format, and, when `exact` is given, must be
    /// exactly that many characters long (a schema's min_length = max_length).
    /// The format is checked by `jmax_format::registry::validate` with the
    /// program's own options, the very call the schema VM makes, so the accept
    /// sets are identical by construction.
    #[inline]
    pub fn format<'c>(
        &mut self,
        ctx: &'c mut NativeContext,
        id: FormatId,
        exact: Option<u32>,
    ) -> NativeResult<&'c str>
    where
        'a: 'c,
    {
        self.skip_ws();
        let max = ctx.limits.syntax.max_string_bytes as usize;
        let (text, end) = self.string_in(max, &mut ctx.arena, self.pos)?;
        jmax_format::registry::validate(id, text.as_bytes(), &ctx.format_options)
            .map_err(|_| Fail)?;
        // Every format accepts ASCII only, so characters and bytes are the same count.
        if exact.is_some_and(|length| text.len() != length as usize) {
            return Err(Fail);
        }
        self.pos = end;
        Ok(text)
    }

    /// A base64 string (standard alphabet, canonical padding, as the `base64`
    /// format defines it) decoded into `out`, which is cleared first.
    pub fn bytea(&mut self, ctx: &mut NativeContext, out: &mut Vec<u8>) -> NativeResult<()> {
        let text = self.format(ctx, FormatId::Base64, None)?.as_bytes();
        out.clear();
        base64_decode(text, out);
        // Format::format advanced the position past the string.
        Ok(())
    }

    /// A string. Without escapes it borrows the input; with escapes it is
    /// decoded once into the context's buffer.
    #[inline]
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
    #[inline]
    pub fn key<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c str>
    where
        'a: 'c,
    {
        let key = self.string(ctx)?;
        self.expect(b':')?;
        Ok(key)
    }

    /// Consume `pattern`, the exact bytes `"name":` of a key with no escapes, if it is next
    /// (after whitespace). A miss consumes nothing but whitespace, so the caller falls through
    /// to `key`. A pattern that `key` would reject (a name over the string limit) or that is
    /// not of the form `"...":` never matches, so a hit is exactly a successful `key`.
    #[inline(always)]
    pub fn match_key(&mut self, ctx: &NativeContext, pattern: &[u8]) -> bool {
        if pattern.len() < 3
            || pattern[0] != b'"'
            || pattern[pattern.len() - 2] != b'"'
            || pattern[pattern.len() - 1] != b':'
            || pattern.len() - 3 > ctx.limits.syntax.max_string_bytes as usize
        {
            return false;
        }
        self.skip_ws();
        if self.input[self.pos..].starts_with(pattern) {
            self.pos += pattern.len();
            true
        } else {
            false
        }
    }

    /// As `match_key`, allowing whitespace before the colon and escaped names. This is
    /// useful to handwritten decoders that do not otherwise parse a key on a miss.
    #[inline(always)]
    pub fn match_key_decoded(&mut self, ctx: &mut NativeContext, pattern: &[u8]) -> bool {
        if pattern.len() < 3
            || pattern[0] != b'"'
            || pattern[pattern.len() - 2] != b'"'
            || pattern[pattern.len() - 1] != b':'
            || pattern.len() - 3 > ctx.limits.syntax.max_string_bytes as usize
        {
            return false;
        }
        self.skip_ws();
        let start = self.pos;
        let key = &pattern[..pattern.len() - 1];
        let rest = &self.input[start..];
        if rest.starts_with(key) {
            let mut end = key.len();
            while matches!(rest.get(end), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                end += 1;
            }
            if rest.get(end) == Some(&b':') {
                self.pos = start + end + 1;
                return true;
            }
        }
        // A valid spelling can differ from the raw key only at an escape.
        // Stop at the first mismatch instead of scanning the remaining value.
        let mut i = 0;
        while i < key.len() {
            match rest.get(i) {
                Some(&byte) if byte == key[i] => i += 1,
                Some(b'\\') => break,
                _ => return false,
            }
        }
        if i == key.len() || rest.get(i) != Some(&b'\\') {
            return false;
        }
        let matched = self
            .key_bytes(ctx)
            .is_ok_and(|decoded| decoded == &key[1..key.len() - 1]);
        if !matched {
            self.pos = start;
        }
        matched
    }

    /// `key` as bytes: the same accept set, and no UTF-8 check of a short ASCII key.
    #[inline]
    pub fn key_bytes<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c [u8]>
    where
        'a: 'c,
    {
        self.skip_ws();
        let max = ctx.limits.syntax.max_string_bytes as usize;
        let (scanned, end) = self.scan_in(max, &mut ctx.arena, self.pos)?;
        self.pos = end;
        self.expect(b':')?;
        Ok(match scanned {
            Scanned::Ascii(bytes) => bytes,
            Scanned::Text(text) => text.as_bytes(),
        })
    }

    /// An object key and its colon, recorded as a key of the innermost object
    /// opened with `NativeContext::begin_object`. Decoders call this on a clone
    /// of the reader taken before `key`, because the key returned by `key`
    /// borrows the context.
    pub fn unknown_key(&mut self, ctx: &mut NativeContext) -> NativeResult<()> {
        self.skip_ws();
        let max = ctx.limits.syntax.max_string_bytes as usize;
        let (scanned, end) = self.scan_in(max, &mut ctx.arena, self.pos)?;
        let text: &[u8] = match scanned {
            Scanned::Ascii(bytes) => bytes,
            Scanned::Text(text) => text.as_bytes(),
        };
        ctx.unknown.push_key_bytes(text)?;
        ctx.unknown_fields = ctx.unknown_fields.checked_add(1).ok_or(Fail)?;
        ctx.unknown_key_bytes = ctx
            .unknown_key_bytes
            .checked_add(text.len() as u64)
            .ok_or(Fail)?;
        self.pos = end;
        self.expect(b':')
    }

    /// The string starting at `start` and the offset just past it; no state changes.
    fn string_at<'c>(
        &self,
        ctx: &'c mut NativeContext,
        start: usize,
    ) -> NativeResult<(&'c str, usize)>
    where
        'a: 'c,
    {
        let max = ctx.limits.syntax.max_string_bytes as usize;
        self.string_in(max, &mut ctx.arena, start)
    }

    /// `string_at` over a bare decoded-text buffer, so a caller can keep using
    /// the rest of the context while the text is borrowed.
    fn string_in<'c>(
        &self,
        max: usize,
        arena: &'c mut Vec<u8>,
        start: usize,
    ) -> NativeResult<(&'c str, usize)>
    where
        'a: 'c,
    {
        match self.scan_in(max, arena, start)? {
            (Scanned::Ascii(bytes), end) => {
                Ok((core::str::from_utf8(bytes).map_err(|_| Fail)?, end))
            }
            (Scanned::Text(text), end) => Ok((text, end)),
        }
    }

    /// The decoded content of the string starting at `start`, and the offset just
    /// past it. A short string of plain ASCII is found by an inline scan of at
    /// most 32 bytes; anything else (an escape, a control or non-ASCII byte, a
    /// longer string) takes the shared string scanner, with the same answers.
    fn scan_in<'c>(
        &self,
        max: usize,
        arena: &'c mut Vec<u8>,
        start: usize,
    ) -> NativeResult<(Scanned<'c>, usize)>
    where
        'a: 'c,
    {
        let input: &'a [u8] = self.input;
        if input.get(start) != Some(&b'"') {
            return Err(Fail);
        }
        let content_start = start + 1;
        let window = &input[content_start..];
        let mut k = 0;
        let reach = window.len().min(32);
        while k < reach {
            let byte = window[k];
            if byte == b'"' {
                if k > max {
                    return Err(Fail);
                }
                return Ok((Scanned::Ascii(&window[..k]), content_start + k + 1));
            }
            if byte == b'\\' || byte < 0x20 || byte >= 0x80 {
                break;
            }
            k += 1;
        }
        let special = jmax_string::scan::first_special(&window[k..]).ok_or(Fail)?;
        let at = content_start + k + special;
        match input[at] {
            b'"' => {
                let content = &input[content_start..at];
                if content.len() > max {
                    return Err(Fail);
                }
                Ok((
                    Scanned::Text(core::str::from_utf8(content).map_err(|_| Fail)?),
                    at + 1,
                ))
            }
            b'\\' => {
                let opening = u32::try_from(start).map_err(|_| Fail)?;
                let scan = jmax_string::scan::scan_string(input, opening).map_err(|_| Fail)?;
                if scan.content.len as usize > max {
                    return Err(Fail);
                }
                arena.clear();
                let lengths = jmax_string::unescape::unescape(input, scan.content, arena)
                    .map_err(|_| Fail)?;
                if lengths.byte_len as usize > max {
                    return Err(Fail);
                }
                let text = core::str::from_utf8(arena).map_err(|_| Fail)?;
                Ok((Scanned::Text(text), scan.next_offset as usize))
            }
            _ => Err(Fail),
        }
    }

    /// Strictly validate and discard one value; `depth` counts enclosing containers.
    pub fn skip_value(&mut self, ctx: &mut NativeContext, depth: u16) -> NativeResult<()> {
        self.skip_ws();
        let start = u32::try_from(self.pos).map_err(|_| Fail)?;
        let limits = ctx.limits.syntax;
        if self.pos <= self.input.len()
            && self.input.len() <= limits.max_payload_bytes as usize
            && depth <= limits.max_depth
            && ctx.numeric.max_exponent_abs >= 0
        {
            let rest = &self.input[self.pos..];
            let literal = if rest.starts_with(b"true") && is_delim(rest.get(4).copied()) {
                Some(4)
            } else if rest.starts_with(b"false") && is_delim(rest.get(5).copied()) {
                Some(5)
            } else if rest.starts_with(b"null") && is_delim(rest.get(4).copied()) {
                Some(4)
            } else {
                None
            };
            if let Some(len) = literal {
                ctx.skip_decoded.clear();
                let end = start.checked_add(len as u32).ok_or(Fail)?;
                self.pos = end as usize;
                ctx.skipped_bytes = ctx
                    .skipped_bytes
                    .checked_add(u64::from(end - start))
                    .ok_or(Fail)?;
                ctx.record_skipped_field()?;
                return Ok(());
            }
        }
        if self.input.len() <= limits.max_payload_bytes as usize
            && depth < limits.max_depth
            && ctx.numeric.max_exponent_abs >= 0
        {
            let flat = self
                .flat_scalar_array(ctx, self.pos)
                .or_else(|| self.flat_scalar_object(ctx, self.pos));
            if let Some((end, decoded)) = flat {
                if decoded <= limits.max_decoded_bytes as usize {
                    let end = u32::try_from(end).map_err(|_| Fail)?;
                    let skipped = ctx
                        .skipped_bytes
                        .checked_add(u64::from(end.checked_sub(start).ok_or(Fail)?))
                        .ok_or(Fail)?;
                    ctx.record_skipped_field()?;
                    ctx.skip_decoded.clear();
                    ctx.skipped_bytes = skipped;
                    self.pos = end as usize;
                    return Ok(());
                }
            }
        }
        let owner = if ctx.limits.syntax.reject_duplicate_keys {
            jmax_abi::syntax::DuplicatePolicyOwner::SchemaVm
        } else {
            jmax_abi::syntax::DuplicatePolicyOwner::Disabled
        };
        ctx.skip_decoded.clear();
        let skip = if self.shared_skip {
            jmax_syntax::skip_strict_continuing
        } else {
            jmax_syntax::skip_strict
        };
        let end = skip(
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
        ctx.skipped_bytes = ctx
            .skipped_bytes
            .checked_add(u64::from(end.checked_sub(start).ok_or(Fail)?))
            .ok_or(Fail)?;
        // One successful strict skip can include a nested subtree. The bound
        // covers disjoint skipped bytes and charges per-skip depth bookkeeping.
        ctx.record_skipped_field()?;
        Ok(())
    }

    /// Validate a flat array of strings and integer tokens without building a
    /// container stack. Unsupported items return `None` so the strict scanner
    /// handles the complete value from its original offset.
    fn flat_scalar_array(&self, ctx: &mut NativeContext, start: usize) -> Option<(usize, usize)> {
        let input = self.input;
        if input.get(start) != Some(&b'[') {
            return None;
        }
        let limits = ctx.limits.syntax;
        let mut at = start + 1;
        while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            at += 1;
        }
        if input.get(at) == Some(&b']') {
            return Some((at + 1, 0));
        }

        let mut items = 0u32;
        let mut decoded = 0usize;
        loop {
            items = items.checked_add(1)?;
            if items > limits.max_array_items {
                return None;
            }
            match input.get(at).copied()? {
                b'"' => {
                    let (scanned, end) = self
                        .scan_in(limits.max_string_bytes as usize, &mut ctx.skip_decoded, at)
                        .ok()?;
                    let len = match scanned {
                        Scanned::Ascii(bytes) => bytes.len(),
                        Scanned::Text(text) => text.len(),
                    };
                    if input[at + 1..end - 1].contains(&b'\\') {
                        decoded = decoded.checked_add(len)?;
                    }
                    if decoded > limits.max_decoded_bytes as usize {
                        return None;
                    }
                    at = end;
                }
                b'-' | b'0'..=b'9' => {
                    let offset = u32::try_from(at).ok()?;
                    let token = jmax_number::lex::lex_number(input, offset, &ctx.numeric).ok()?;
                    if !matches!(
                        token.kind(),
                        NumberLexKind::UnsignedInt | NumberLexKind::SignedInt
                    ) || token.integer_digits() > u32::from(limits.max_number_digits)
                    {
                        return None;
                    }
                    let raw = token.raw_bytes();
                    if ctx.numeric.reject_negative_zero && raw.starts_with(b"-0") {
                        return None;
                    }
                    at = at.checked_add(token.span().len as usize)?;
                    if !is_delim(input.get(at).copied()) {
                        return None;
                    }
                }
                _ => return None,
            }
            while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                at += 1;
            }
            match input.get(at) {
                Some(b',') => {
                    at += 1;
                    while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                        at += 1;
                    }
                }
                Some(b']') => return Some((at + 1, decoded)),
                _ => return None,
            }
        }
    }

    /// Validate a flat scalar object when duplicate rejection is disabled.
    /// With duplicate rejection enabled, the general scanner owns key tracking.
    fn flat_scalar_object(&self, ctx: &mut NativeContext, start: usize) -> Option<(usize, usize)> {
        let input = self.input;
        let limits = ctx.limits.syntax;
        if input.get(start) != Some(&b'{') || limits.reject_duplicate_keys {
            return None;
        }
        let mut at = start + 1;
        let mut decoded = 0usize;
        let mut fields = 0u32;
        while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            at += 1;
        }
        if input.get(at) == Some(&b'}') {
            return Some((at + 1, 0));
        }
        loop {
            fields = fields.checked_add(1)?;
            if fields > limits.max_fields_per_object {
                return None;
            }
            let (key, end) = self
                .scan_in(limits.max_string_bytes as usize, &mut ctx.skip_decoded, at)
                .ok()?;
            let key_len = match key {
                Scanned::Ascii(bytes) => bytes.len(),
                Scanned::Text(text) => text.len(),
            };
            if input[at + 1..end - 1].contains(&b'\\') {
                decoded = decoded.checked_add(key_len)?;
            }
            at = end;
            while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                at += 1;
            }
            if input.get(at) != Some(&b':') {
                return None;
            }
            at += 1;
            while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                at += 1;
            }
            match input.get(at).copied()? {
                b'"' => {
                    let (value, end) = self
                        .scan_in(limits.max_string_bytes as usize, &mut ctx.skip_decoded, at)
                        .ok()?;
                    let len = match value {
                        Scanned::Ascii(bytes) => bytes.len(),
                        Scanned::Text(text) => text.len(),
                    };
                    decoded = decoded.checked_add(len)?;
                    at = end;
                }
                b't' if input.get(at..at.checked_add(4)?) == Some(b"true") => {
                    at += 4;
                    if !is_delim(input.get(at).copied()) {
                        return None;
                    }
                }
                b'f' if input.get(at..at.checked_add(5)?) == Some(b"false") => {
                    at += 5;
                    if !is_delim(input.get(at).copied()) {
                        return None;
                    }
                }
                b'n' if input.get(at..at.checked_add(4)?) == Some(b"null") => {
                    at += 4;
                    if !is_delim(input.get(at).copied()) {
                        return None;
                    }
                }
                b'-' | b'0'..=b'9' => {
                    let offset = u32::try_from(at).ok()?;
                    let token = jmax_number::lex::lex_number(input, offset, &ctx.numeric).ok()?;
                    if !matches!(
                        token.kind(),
                        NumberLexKind::UnsignedInt | NumberLexKind::SignedInt
                    ) {
                        return None;
                    }
                    let digits = token
                        .raw_bytes()
                        .iter()
                        .filter(|byte| byte.is_ascii_digit())
                        .count();
                    if digits > usize::from(limits.max_number_digits) {
                        return None;
                    }
                    at = at.checked_add(token.span().len as usize)?;
                    if !is_delim(input.get(at).copied()) {
                        return None;
                    }
                }
                _ => return None,
            }
            if decoded > limits.max_decoded_bytes as usize {
                return None;
            }
            while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                at += 1;
            }
            match input.get(at) {
                Some(b',') => {
                    at += 1;
                    while matches!(input.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                        at += 1;
                    }
                }
                Some(b'}') => return Some((at + 1, decoded)),
                _ => return None,
            }
        }
    }
}

fn convert(token: &NumberToken<'_>, digits_limit: u32) -> NativeResult<i128> {
    if !matches!(
        token.kind(),
        NumberLexKind::UnsignedInt | NumberLexKind::SignedInt
    ) {
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

const POW10: [u128; 39] = {
    let mut table = [1u128; 39];
    let mut i = 1;
    while i < 39 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// The common decimal, read straight from the input: `"`, an optional `-`,
/// `0` or a digit run not starting with `0`, an optional `.` and at least one
/// digit, `"`, with at most 18 digits in all. `pos` is the opening quote.
///
/// Returns `Some` only when the general path (`parse_decimal` on the content
/// of the string) would return exactly the same value: no exponent, escape or
/// negative zero, no downward rescale (so no inexactness), a coefficient that
/// fits the precision and tick of the policy, and a policy `validate_policy`
/// accepts. In every other case, including each case where the general path
/// rejects, it returns `None` and the caller defers to the general path.
#[inline(always)]
fn fast_decimal(
    input: &[u8],
    pos: usize,
    policy: &DecimalPolicy,
    digits_limit: usize,
    max_string: usize,
) -> Option<(Decimal128, usize)> {
    if policy.max_precision == 0 || policy.max_precision > 39 || policy.tick_step < 0 {
        return None;
    }
    if *input.get(pos)? != b'"' {
        return None;
    }
    let start = pos + 1;
    let mut i = start;
    let negative = *input.get(i)? == b'-';
    i += usize::from(negative);
    let mut magnitude: u64 = 0;
    let mut digits = 0usize;
    match *input.get(i)? {
        b'0' => {
            i += 1;
            digits = 1;
        }
        b'1'..=b'9' => {
            while let Some(&digit @ b'0'..=b'9') = input.get(i) {
                if digits == 18 {
                    return None;
                }
                magnitude = magnitude * 10 + u64::from(digit - b'0');
                digits += 1;
                i += 1;
            }
        }
        _ => return None,
    }
    let mut fraction = 0usize;
    if input.get(i) == Some(&b'.') {
        i += 1;
        // Eight-place wire decimals are common. Validate all eight bytes,
        // then combine pairs and groups without a dependent multiply per byte.
        let eight = if digits <= 10 && input.get(i + 8) == Some(&b'"') {
            input
                .get(i..i + 8)
                .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
                .filter(|word| {
                    ((word.wrapping_sub(0x3030_3030_3030_3030)
                        | word.wrapping_add(0x4646_4646_4646_4646))
                        & 0x8080_8080_8080_8080)
                        == 0
                })
        } else {
            None
        };
        if let Some(word) = eight {
            let values = word - 0x3030_3030_3030_3030;
            let pairs = (values.wrapping_mul(10).wrapping_add(values >> 8)) & 0x00ff_00ff_00ff_00ff;
            let groups =
                (pairs.wrapping_mul(100).wrapping_add(pairs >> 16)) & 0x0000_ffff_0000_ffff;
            let value = groups.wrapping_mul(10_000).wrapping_add(groups >> 32) as u32;
            magnitude = magnitude * 100_000_000 + u64::from(value);
            digits += 8;
            fraction = 8;
            i += 8;
        } else {
            while let Some(&digit @ b'0'..=b'9') = input.get(i) {
                if digits == 18 {
                    return None;
                }
                magnitude = magnitude * 10 + u64::from(digit - b'0');
                digits += 1;
                fraction += 1;
                i += 1;
            }
        }
        if fraction == 0 {
            return None;
        }
    }
    if *input.get(i)? != b'"' {
        return None;
    }
    if digits > digits_limit || i - start > max_string || (negative && magnitude == 0) {
        return None;
    }
    let scale = usize::from(policy.target_scale);
    if fraction > scale || digits + (scale - fraction) > 38 {
        return None;
    }
    let coefficient = u128::from(magnitude) * POW10[scale - fraction];
    if policy.max_precision < 39 && coefficient >= POW10[usize::from(policy.max_precision)] {
        return None;
    }
    let coefficient = coefficient as i128;
    if policy.tick_step > 1 && coefficient % policy.tick_step != 0 {
        return None;
    }
    let coefficient = if negative { -coefficient } else { coefficient };
    Some((Decimal128::new(coefficient, policy.target_scale), i + 1))
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

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use jmax_abi::number::NumericPolicy;
    use std::{format, string::String, vec::Vec};

    #[test]
    fn native_budget_counters_cover_enum_reads_and_nested_skips() {
        let mut ctx =
            NativeContext::new(crate::VmLimits::default(), NumericPolicy::strict_default());
        ctx.record_enum_value().unwrap();
        assert_eq!(Reader::new(b"\"sell\"").string(&mut ctx), Ok("sell"));
        assert!(ctx.enum_values >= 1);

        ctx.record_enum_value().unwrap();
        assert_eq!(
            Reader::new(b"2").int(&mut ctx, i32::MIN as i64, i32::MAX as i64),
            Ok(2)
        );
        assert!(ctx.enum_values >= 2);

        let mut reader = Reader::new(br#"{"nested":{"x":1},"items":[true,false]}"#);
        reader.skip_value(&mut ctx, 0).unwrap();
        assert_eq!(ctx.skipped_fields, 1);
    }

    #[test]
    fn empty_object_accepts_only_empty_objects_and_rewinds_on_failure() {
        let mut reader = Reader::new(b" { \n } ");
        assert_eq!(reader.empty_object(), Ok(()));
        assert_eq!(reader.pos(), 6);

        for input in [b"{}".as_slice(), b"{ \n\t}"] {
            assert_eq!(Reader::new(input).empty_object(), Ok(()));
        }
        for input in [b"null".as_slice(), b"{\"x\":0}", b"["] {
            let mut reader = Reader::new(input);
            assert_eq!(reader.empty_object(), Err(Fail));
            assert_eq!(reader.pos(), 0);
        }
    }

    /// If `fast_decimal` answers, the general path answers the same value.
    fn agree(text: &str, policy: &DecimalPolicy, digits_limit: usize, max_string: usize) -> bool {
        let quoted = format!("\"{text}\"");
        let Some((value, end)) =
            fast_decimal(quoted.as_bytes(), 0, policy, digits_limit, max_string)
        else {
            return false;
        };
        assert_eq!(end, quoted.len(), "{text}");
        for reject_negative_zero in [true, false] {
            let numeric = NumericPolicy {
                reject_negative_zero,
                ..NumericPolicy::strict_default()
            };
            let slow = parse_decimal(text.as_bytes(), policy, &numeric, digits_limit);
            assert_eq!(
                slow,
                Ok(value),
                "{text} {policy:?} {digits_limit} {max_string} {reject_negative_zero}"
            );
        }
        assert!(text.len() <= max_string, "{text}");
        true
    }

    fn policies() -> Vec<DecimalPolicy> {
        let mut all = Vec::new();
        for target_scale in [0u8, 1, 2, 8, 17, 18, 30, 38, 40] {
            for max_precision in [0u8, 1, 2, 9, 10, 18, 19, 38, 39, 40] {
                for tick_step in [-1i128, 0, 1, 2, 5, 7, 10, 100_000_000] {
                    for reject_inexact_rescale in [true, false] {
                        for allow_exponent in [true, false] {
                            all.push(DecimalPolicy {
                                max_precision,
                                target_scale,
                                allow_exponent,
                                reject_inexact_rescale,
                                tick_step,
                            });
                        }
                    }
                }
            }
        }
        all
    }

    #[test]
    fn exhaustive_fast_decimal_matches_slow() {
        let spot = DecimalPolicy::spot_8();
        let tick = DecimalPolicy {
            tick_step: 5,
            max_precision: 9,
            ..spot
        };
        let wide = DecimalPolicy {
            target_scale: 2,
            max_precision: 39,
            ..spot
        };
        let zero = DecimalPolicy {
            target_scale: 0,
            ..spot
        };
        let alphabet = *b"-0159.";
        let mut taken = 0usize;
        let mut buffer = Vec::new();
        for length in 1..=7usize {
            let mut counter = std::vec![0usize; length];
            loop {
                buffer.clear();
                buffer.extend(counter.iter().map(|&i| alphabet[i]));
                let text = core::str::from_utf8(&buffer).unwrap();
                for policy in [&spot, &tick, &wide, &zero] {
                    taken += usize::from(agree(text, policy, 18, 1 << 20));
                }
                taken += usize::from(agree(text, &spot, 3, 6));
                let mut at = 0;
                loop {
                    counter[at] += 1;
                    if counter[at] < alphabet.len() {
                        break;
                    }
                    counter[at] = 0;
                    at += 1;
                    if at == length {
                        break;
                    }
                }
                if at == length {
                    break;
                }
            }
        }
        assert!(taken > 10_000, "{taken}");
    }

    #[test]
    fn structured_fast_decimal_sweep_matches_slow() {
        let policies = policies();
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut taken = 0usize;
        for sign in ["", "-"] {
            for integer in 0..=19usize {
                for fraction in 0..=20usize {
                    for pattern in 0..4 {
                        let digits = |count: usize, first: bool, state: &mut u64| -> String {
                            (0..count)
                                .map(|k| match pattern {
                                    0 => '9',
                                    1 => {
                                        if k == 0 && first {
                                            '1'
                                        } else {
                                            '0'
                                        }
                                    }
                                    2 => '0',
                                    _ => {
                                        *state = state
                                            .wrapping_mul(6364136223846793005)
                                            .wrapping_add(1442695040888963407);
                                        char::from(b'0' + (*state >> 33) as u8 % 10)
                                    }
                                })
                                .collect()
                        };
                        let mut whole = digits(integer, true, &mut state);
                        if integer == 0 {
                            whole = String::from("0");
                        } else if whole.starts_with('0') && integer > 1 {
                            whole.replace_range(0..1, "7");
                        }
                        let mut text = format!("{sign}{whole}");
                        if fraction > 0 {
                            text.push('.');
                            text.push_str(&digits(fraction, false, &mut state));
                        }
                        // A sample of the policy product per token keeps the debug run short.
                        let offset = (state >> 20) as usize;
                        for step in 0..120 {
                            let policy = &policies[(offset + step * 97) % policies.len()];
                            for (limit, max_string) in
                                [(3usize, 6usize), (18, 1 << 20), (64, 1 << 20)]
                            {
                                taken += usize::from(agree(&text, policy, limit, max_string));
                            }
                        }
                        taken += usize::from(agree(&text, &DecimalPolicy::spot_8(), 18, 1 << 20));
                    }
                }
            }
        }
        assert!(taken > 5_000, "{taken}");
    }

    #[test]
    fn fast_path_is_taken_for_canonical_tokens() {
        let spot = DecimalPolicy::spot_8();
        for (text, coefficient) in [
            ("0.0024", 240_000i128),
            ("10.5", 1_050_000_000),
            ("20039.14430000", 2_003_914_430_000),
            ("-1.5", -150_000_000),
            ("0.00000001", 1),
            ("0", 0),
            ("123456789012345678", 12_345_678_901_234_567_800_000_000),
        ] {
            let quoted = format!("\"{text}\"");
            let got = fast_decimal(quoted.as_bytes(), 0, &spot, 18, 1 << 20);
            assert_eq!(
                got,
                Some((Decimal128::new(coefficient, 8), quoted.len())),
                "{text}"
            );
        }
        for text in [
            "-0",
            "-0.0",
            "01",
            "1.",
            ".5",
            "1e2",
            "+1",
            "1.123456789",
            " 1",
            "1 ",
            "\\u0031",
            "1234567890123456789",
        ] {
            let quoted = format!("\"{text}\"");
            assert_eq!(
                fast_decimal(quoted.as_bytes(), 0, &spot, 18, 1 << 20),
                None,
                "{text}"
            );
        }
    }
}
