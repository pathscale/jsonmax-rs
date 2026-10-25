//! Clean-token specialization of the strict tape grammar.
use super::*;
pub(super) fn build_inner(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Summary> {
    build_mode::<true>(raw, limits, numeric, scratch)
}
#[inline(never)]
fn build_mode<const FAST: bool>(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
) -> Result<Summary> {
    let mut shapes = core::mem::take(&mut scratch.shapes);
    shapes.reset();
    let result = build_shaped::<FAST>(raw, limits, numeric, scratch, &mut shapes);
    scratch.shapes = shapes;
    result
}
#[inline(always)]
fn build_shaped<const FAST: bool>(
    raw: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    scratch: &mut DomScratch,
    shapes: &mut Shapes,
) -> Result<Summary> {
    let mut cursor = FastDomCursor::<FAST>(BlockCursor::new(
        raw,
        limits,
        numeric,
        DuplicatePolicyOwner::Syntax,
    )?);
    let mut summary = Summary::default();
    append_value(&mut cursor, scratch, &mut summary, shapes, 0, None)?;
    while let Some(mut frame) = scratch.stack.last().copied() {
        let mut byte = cursor.skip_whitespace()?.ok_or_else(|| {
            syntax_error(
                ErrorCode::UnexpectedEof,
                cursor.offset(),
                "unterminated JSON container",
            )
        })?;
        let closing = if frame.tag == TAG_OBJECT { b'}' } else { b']' };
        if frame.phase == Phase::AfterValue {
            if byte == closing {
                close(&mut cursor, scratch, shapes)?;
                continue;
            }
            if byte == b',' {
                cursor.advance(1);
            } else {
                cursor.expect_byte(b',', "expected comma or closing container delimiter")?;
            }
            if let Some(top) = scratch.stack.last_mut() {
                top.phase = Phase::Next;
            }
            frame.phase = Phase::Next;
            byte = cursor.skip_whitespace()?.ok_or_else(|| {
                syntax_error(
                    ErrorCode::UnexpectedEof,
                    cursor.offset(),
                    "unterminated JSON container",
                )
            })?;
        }
        if byte == closing && frame.phase == Phase::First {
            close(&mut cursor, scratch, shapes)?;
            continue;
        }
        let mut key_flags = 0;
        if frame.tag == TAG_OBJECT {
            check_count(frame, limits, cursor.offset())?;
            if byte != b'"' {
                return Err(syntax_error(
                    ErrorCode::InvalidSyntax,
                    cursor.offset(),
                    "expected an object member key",
                ));
            }
            let matched = if limits.reject_duplicate_keys {
                match_shape_token(
                    shapes,
                    scratch.stack.len() - 1,
                    raw,
                    cursor.offset(),
                    limits,
                )
            } else {
                None
            };
            let matched_shape_token = matched.is_some();
            let string = if let Some(string) = matched {
                cursor.advance(string.raw_token.len);
                string
            } else {
                scan_string_inline(&mut cursor, &mut scratch.decoded)?
            };
            let key_index = tape_index(scratch.tape.len(), string.raw_token.offset)?;
            append_string(&mut scratch.tape, TAPE_TAG_KEY, string, 0, NO_NODE)?;
            if limits.reject_duplicate_keys && !matched_shape_token {
                let depth = scratch.stack.len() - 1;
                if shapes.get(depth).is_none_or(|shape| shape.disabled) {
                    let hash = key_hash(string_bytes(
                        raw,
                        &scratch.decoded,
                        string.cooked.span,
                        string.cooked.source,
                    ));
                    check_duplicate(
                        scratch,
                        raw,
                        depth,
                        hash,
                        string.cooked,
                        string.raw_token.offset,
                    )?;
                } else {
                    // The hash only places keys in this builder's private table;
                    // matches are always confirmed on the cooked bytes.
                    let (hash, matched_shape, matched_prefix_len) = shape_hash(
                        shapes,
                        depth,
                        raw,
                        &scratch.decoded,
                        string.cooked,
                        string.raw_token,
                        string.is_ascii,
                        string.raw_token.offset,
                    )?;
                    if !matched_shape {
                        let seed = !shapes[depth].table_seeded;
                        shapes[depth].table_seeded = true;
                        if seed {
                            seed_shape_prefix(
                                scratch,
                                shapes,
                                raw,
                                depth,
                                matched_prefix_len,
                                string.raw_token.offset,
                            )?;
                        }
                        check_duplicate(
                            scratch,
                            raw,
                            depth,
                            hash,
                            string.cooked,
                            string.raw_token.offset,
                        )?;
                    }
                }
            }
            key_flags = TAPE_FLAG_HAS_KEY;
            if scratch.tape[key_index as usize].has_flag(TAPE_FLAG_EXTENDED) {
                key_flags |= TAPE_FLAG_KEY_EXTENDED;
            }
            if cursor.skip_whitespace()? == Some(b':') {
                cursor.advance(1);
            } else {
                cursor.expect_byte(b':', "expected colon after object member key")?;
            }
        } else {
            check_count(frame, limits, cursor.offset())?;
        }
        let value_byte = (frame.tag == TAG_ARRAY).then_some(byte);
        append_value(
            &mut cursor,
            scratch,
            &mut summary,
            shapes,
            key_flags,
            value_byte,
        )?;
    }
    cursor.finish_eof()?;
    Ok(summary)
}
#[inline(always)]
fn append_value<const FAST: bool>(
    cursor: &mut FastDomCursor<'_, FAST>,
    scratch: &mut DomScratch,
    summary: &mut Summary,
    shapes: &mut Shapes,
    key_flags: u32,
    value_byte: Option<u8>,
) -> Result<()> {
    let byte = match value_byte {
        Some(byte) => Some(byte),
        None => cursor.skip_whitespace()?,
    }
    .ok_or_else(|| {
        syntax_error(
            ErrorCode::UnexpectedEof,
            cursor.offset(),
            "expected a JSON value",
        )
    })?;
    let offset = cursor.offset();
    let index = tape_index(scratch.tape.len(), offset)?;
    let parent = scratch.stack.last().map_or(NO_NODE, |frame| frame.node);
    if byte == b'[' && try_coordinate_ring(cursor, scratch, summary, key_flags, offset, parent)? {
        commit_child(scratch);
        summary.nodes = increment(summary.nodes, offset)?;
        return Ok(());
    }
    if byte == b'[' && try_scalar_array(cursor, scratch, summary, key_flags, offset, parent)? {
        commit_child(scratch);
        summary.nodes = increment(summary.nodes, offset)?;
        return Ok(());
    }
    match byte {
        b'{' | b'[' => {
            if scratch.stack.len() >= cursor.limits().max_depth as usize {
                return Err(syntax_error(
                    ErrorCode::DepthLimitExceeded,
                    offset,
                    "JSON container exceeds depth limit",
                ));
            }
            let tag = if byte == b'{' { TAG_OBJECT } else { TAG_ARRAY };
            if tag == TAG_OBJECT && cursor.limits().reject_duplicate_keys {
                shape_open(shapes, scratch.stack.len(), offset)?;
            }
            reserve(&mut scratch.tape, 3, offset)?;
            scratch.stack.reserve(1, offset)?;
            cursor.advance(1);
            scratch
                .tape
                .push(TapeWord::pack_header(tag, key_flags, offset));
            scratch.tape.push(TapeWord::pack_pair(parent, index + 3));
            scratch.tape.push(TapeWord::pack_pair(0, offset + 1));
            commit_child(scratch);
            scratch.stack.push(OpenContainer {
                node: index,
                tag,
                count: 0,
                phase: Phase::First,
            });
            summary.containers = increment(summary.containers, offset)?;
            summary.max_depth = summary.max_depth.max(scratch.stack.len() as u16);
        }
        b'"' => {
            let meta = scan_string_inline(cursor, &mut scratch.decoded)?;
            append_string(&mut scratch.tape, TAG_STRING, meta, key_flags, parent)?;
            commit_child(scratch);
        }
        b'-' | b'0'..=b'9' => {
            let span = cursor.scan_number_span()?;
            append_number(&mut scratch.tape, span, key_flags, parent)?;
            commit_child(scratch);
        }
        b'n' | b't' | b'f' => {
            let (kind, span) = cursor.scan_literal()?;
            let tag = match kind {
                TokenKind::Null => TAG_NULL,
                TokenKind::False => TAG_FALSE,
                TokenKind::True => TAG_TRUE,
                _ => {
                    return Err(dom_error(
                        ErrorCode::InternalError,
                        offset,
                        "BlockCursor returned a nonliteral kind",
                    ));
                }
            };
            reserve(&mut scratch.tape, 2, offset)?;
            scratch.tape.push(TapeWord::pack_header(
                tag,
                key_flags | span.len,
                span.offset,
            ));
            scratch.tape.push(TapeWord::pack_pair(parent, index + 2));
            commit_child(scratch);
        }
        _ => {
            return Err(syntax_error(
                ErrorCode::InvalidSyntax,
                offset,
                "expected a JSON value",
            ));
        }
    }
    summary.nodes = increment(summary.nodes, offset)?;
    Ok(())
}

/// Inline the clean ASCII string scan; escaped, control, and non-ASCII tokens
/// retain BlockCursor's exact decoder and diagnostics.
#[inline(always)]
fn scan_string_inline<const FAST: bool>(
    cursor: &mut FastDomCursor<'_, FAST>,
    decoded: &mut Vec<u8>,
) -> Result<StringMeta> {
    if !FAST {
        return cursor.0.scan_string_meta(decoded);
    }
    let start = cursor.offset() as usize;
    if cursor.peek_byte() == Some(b'"') {
        let body_start = start + 1;
        let tail = &cursor.input()[body_start..];
        let (len, ascii) = string_special_ascii(tail);
        if tail.get(len) == Some(&b'"') && len <= cursor.limits().max_string_bytes as usize {
            // The boundary scan already excluded escapes and controls. Count
            // valid raw Unicode here instead of rediscovering the token.
            let scalar_len = if ascii {
                len as u32
            } else {
                let Ok(count) = jmax_utf8::validate_and_count_scalars(&tail[..len], body_start as u32)
                else {
                    return cursor.0.scan_string_meta_uncached(decoded);
                };
                count
            };
            let raw_content = Span32::new(body_start as u32, len as u32);
            cursor.advance(len as u32 + 2);
            return Ok(StringMeta {
                raw_content,
                raw_token: Span32::new(start as u32, len as u32 + 2),
                cooked: StrRef {
                    span: raw_content,
                    source: SpanSource::RawPayload,
                    scalar_len,
                },
                is_unescaped: true,
                is_ascii: ascii,
            });
        }
    }
    cursor.0.scan_string_meta_uncached(decoded)
}

/// Fast path for arrays of two-number coordinate rows. Tape writes are rolled
/// back if a later row disproves the candidate, so the normal grammar cursor
/// remains responsible for every nonmatching input and diagnostic.
#[inline(never)]
fn try_coordinate_ring<const FAST: bool>(
    cursor: &mut FastDomCursor<'_, FAST>,
    scratch: &mut DomScratch,
    summary: &mut Summary,
    key_flags: u32,
    offset: u32,
    parent: u32,
) -> Result<bool> {
    if scratch
        .stack
        .len()
        .checked_add(2)
        .is_none_or(|depth| depth > cursor.limits().max_depth as usize)
        || cursor.limits().max_array_items < 2
    {
        return Ok(false);
    }
    let input = cursor.input();
    let start = cursor.offset() as usize;
    let tape_start = scratch.tape.len();
    let mut p = skip_ws(input, start + 1);
    if input.get(p) != Some(&b'[') {
        return Ok(false);
    }
    let outer_index = tape_index(scratch.tape.len(), offset)?;
    reserve(&mut scratch.tape, 3, offset)?;
    let outer_header = scratch.tape.len();
    scratch
        .tape
        .push(TapeWord::pack_header(TAG_ARRAY, key_flags, offset));
    scratch
        .tape
        .push(TapeWord::pack_pair(parent, outer_index + 3));
    scratch.tape.push(TapeWord::pack_pair(0, 0));
    let mut rows = 0u32;
    loop {
        if input.get(p) != Some(&b'[') || rows >= cursor.limits().max_array_items {
            scratch.tape.truncate(tape_start);
            return Ok(false);
        }
        let row_offset = p as u32;
        let row_index = tape_index(scratch.tape.len(), row_offset)?;
        reserve(&mut scratch.tape, 3, row_offset)?;
        let row_header = scratch.tape.len();
        scratch
            .tape
            .push(TapeWord::pack_header(TAG_ARRAY, 0, row_offset));
        scratch
            .tape
            .push(TapeWord::pack_pair(outer_index, row_index + 3));
        scratch.tape.push(TapeWord::pack_pair(0, 0));
        p = skip_ws(input, p + 1);
        let Some(first) =
            coordinate_number(input, &mut p, cursor.limits(), cursor.numeric_policy())
        else {
            scratch.tape.truncate(tape_start);
            return Ok(false);
        };
        append_number(&mut scratch.tape, first, 0, row_index)?;
        p = skip_ws(input, p);
        if input.get(p) != Some(&b',') {
            scratch.tape.truncate(tape_start);
            return Ok(false);
        }
        p = skip_ws(input, p + 1);
        let Some(second) =
            coordinate_number(input, &mut p, cursor.limits(), cursor.numeric_policy())
        else {
            scratch.tape.truncate(tape_start);
            return Ok(false);
        };
        append_number(&mut scratch.tape, second, 0, row_index)?;
        p = skip_ws(input, p);
        if input.get(p) != Some(&b']') {
            scratch.tape.truncate(tape_start);
            return Ok(false);
        }
        p += 1;
        rows += 1;
        let row_end = tape_index(scratch.tape.len(), p as u32)?;
        scratch.tape[row_header + 1] = TapeWord::pack_pair(outer_index, row_end);
        scratch.tape[row_header + 2] = TapeWord::pack_pair(2, p as u32);
        p = skip_ws(input, p);
        match input.get(p) {
            Some(b',') => p = skip_ws(input, p + 1),
            Some(b']') => {
                p += 1;
                break;
            }
            _ => {
                scratch.tape.truncate(tape_start);
                return Ok(false);
            }
        }
    }
    let end = p;
    cursor.advance((end - start) as u32);
    let outer_end = tape_index(scratch.tape.len(), end as u32)?;
    scratch.tape[outer_header + 2] = TapeWord::pack_pair(rows, end as u32);
    scratch.tape[outer_header + 1] = TapeWord::pack_pair(parent, outer_end);
    summary.nodes = summary
        .nodes
        .checked_add(rows.checked_mul(3).ok_or_else(|| {
            dom_error(
                ErrorCode::PayloadTooLarge,
                offset,
                "DOM counter exceeds u32",
            )
        })?)
        .ok_or_else(|| {
            dom_error(
                ErrorCode::PayloadTooLarge,
                offset,
                "DOM counter exceeds u32",
            )
        })?;
    summary.containers = summary
        .containers
        .checked_add(rows.checked_add(1).ok_or_else(|| {
            dom_error(
                ErrorCode::PayloadTooLarge,
                offset,
                "DOM counter exceeds u32",
            )
        })?)
        .ok_or_else(|| {
            dom_error(
                ErrorCode::PayloadTooLarge,
                offset,
                "DOM counter exceeds u32",
            )
        })?;
    summary.max_depth = summary.max_depth.max((scratch.stack.len() + 2) as u16);
    Ok(true)
}

/// Flat scalar arrays emit records in one loop without creating a frame or
/// dispatching every delimiter through the container state machine.
fn try_scalar_array<const FAST: bool>(
    cursor: &mut FastDomCursor<'_, FAST>,
    scratch: &mut DomScratch,
    summary: &mut Summary,
    key_flags: u32,
    offset: u32,
    parent: u32,
) -> Result<bool> {
    if scratch.stack.len() >= usize::from(cursor.limits().max_depth) {
        return Ok(false);
    }
    let input = cursor.input();
    let saved = scratch.tape.len();
    let index = tape_index(saved, offset)?;
    let limits = cursor.limits();
    let mut at = skip_ws(input, offset as usize + 1);
    // Reject nested candidates before writing anything.
    if matches!(input.get(at), Some(b'[' | b'{')) {
        return Ok(false);
    }
    reserve(&mut scratch.tape, 3, offset)?;
    scratch
        .tape
        .push(TapeWord::pack_header(TAG_ARRAY, key_flags, offset));
    scratch.tape.push(TapeWord::pack_pair(parent, index + 3));
    scratch.tape.push(TapeWord::pack_pair(0, 0));
    let mut count = 0u32;
    let accepted = (|| -> Result<bool> {
        if input.get(at) != Some(&b']') {
            loop {
                if count >= limits.max_array_items {
                    return Ok(false);
                }
                let value_offset = at as u32;
                match input.get(at) {
                    Some(b'-' | b'0'..=b'9') => {
                        let Some(span) =
                            coordinate_number(input, &mut at, limits, cursor.numeric_policy())
                        else {
                            return Ok(false);
                        };
                        append_number(&mut scratch.tape, span, 0, index)?;
                    }
                    Some(b'"') => {
                        let (len, ascii) = string_special_ascii(&input[at + 1..]);
                        if !ascii
                            || len > limits.max_string_bytes as usize
                            || input.get(at + 1 + len) != Some(&b'"')
                        {
                            return Ok(false);
                        }
                        let body = Span32::new(value_offset + 1, len as u32);
                        append_string(
                            &mut scratch.tape,
                            TAG_STRING,
                            StringMeta {
                                raw_content: body,
                                raw_token: Span32::new(value_offset, len as u32 + 2),
                                cooked: StrRef {
                                    span: body,
                                    source: SpanSource::RawPayload,
                                    scalar_len: len as u32,
                                },
                                is_unescaped: true,
                                is_ascii: true,
                            },
                            0,
                            index,
                        )?;
                        at += len + 2;
                    }
                    Some(b't' | b'f' | b'n') => {
                        let (text, tag): (&[u8], u8) = match input[at] {
                            b't' => (b"true", TAG_TRUE),
                            b'f' => (b"false", TAG_FALSE),
                            _ => (b"null", TAG_NULL),
                        };
                        if input.get(at..at + text.len()) != Some(text) {
                            return Ok(false);
                        }
                        let node = tape_index(scratch.tape.len(), value_offset)?;
                        reserve(&mut scratch.tape, 2, value_offset)?;
                        scratch.tape.push(TapeWord::pack_header(
                            tag,
                            text.len() as u32,
                            value_offset,
                        ));
                        scratch.tape.push(TapeWord::pack_pair(index, node + 2));
                        at += text.len();
                    }
                    _ => return Ok(false),
                }
                count += 1;
                at = skip_ws(input, at);
                match input.get(at) {
                    Some(b',') => {
                        at = skip_ws(input, at + 1);
                        if input.get(at) == Some(&b']') {
                            return Ok(false);
                        }
                    }
                    Some(b']') => break,
                    _ => return Ok(false),
                }
            }
        }
        Ok(input.get(at) == Some(&b']'))
    })()?;
    if !accepted {
        scratch.tape.truncate(saved);
        return Ok(false);
    }
    at += 1;
    let end = tape_index(scratch.tape.len(), at as u32)?;
    scratch.tape[saved + 1] = TapeWord::pack_pair(parent, end);
    scratch.tape[saved + 2] = TapeWord::pack_pair(count, at as u32);
    cursor.advance((at - offset as usize) as u32);
    summary.nodes = summary.nodes.checked_add(count).ok_or_else(|| {
        dom_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "DOM counter exceeds u32",
        )
    })?;
    summary.containers = increment(summary.containers, offset)?;
    summary.max_depth = summary.max_depth.max((scratch.stack.len() + 1) as u16);
    Ok(true)
}

/// SWAR digit runs plus a bounded decimal fraction; uncommon number forms
/// fall back to the established cursor grammar.
fn coordinate_number(
    input: &[u8],
    p: &mut usize,
    limits: Limits,
    numeric: Option<NumericPolicy>,
) -> Option<Span32> {
    let start = *p;
    if input.get(*p) == Some(&b'-') {
        *p += 1;
    }
    let int_start = *p;
    match input.get(*p)? {
        b'0' => *p += 1,
        b'1'..=b'9' => *p = digit_run_end(input, *p),
        _ => return None,
    }
    if input.get(int_start) == Some(&b'0') && input.get(*p).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let mut digits = *p - int_start;
    let is_float = input.get(*p) == Some(&b'.');
    if is_float {
        *p += 1;
        let frac_start = *p;
        *p = digit_run_end(input, *p);
        if *p == frac_start {
            return None;
        }
        digits += *p - frac_start;
    }
    if digits > usize::from(limits.max_number_digits)
        || !matches!(
            input.get(*p),
            Some(b',' | b']' | b' ' | b'\t' | b'\r' | b'\n')
        )
    {
        return None;
    }
    if let Some(policy) = numeric {
        let token = &input[start..*p];
        let negative = token.first() == Some(&b'-');
        let negative_zero = negative && token[1..].iter().all(|&byte| byte == b'0' || byte == b'.');
        // Mirror strict_fast's policy-safe window. Exponents are already
        // excluded by this recognizer; long floats and negative zero go back
        // through BlockCursor so its exact numeric policy check stays in charge.
        if (policy.reject_negative_zero && negative_zero)
            || (is_float && token.len() > POLICY_SAFE_FLOAT_BYTES)
        {
            return None;
        }
    }
    Some(Span32::new(start as u32, (*p - start) as u32))
}

const POLICY_SAFE_FLOAT_BYTES: usize = 300;

/// Find the first JSON string boundary while checking ASCII in the same pass.
/// The SWAR prefilter is bounded to complete words; candidate words are
/// inspected bytewise to avoid false positives from the zero-byte idiom.
#[inline(always)]
fn string_special_ascii(input: &[u8]) -> (usize, bool) {
    #[cfg(target_arch = "aarch64")]
    {
        unsafe { special_ascii_neon(input) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        jmax_dispatch::short::string_special_ascii(input)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn scalar_dom_ascii(input: &[u8]) -> (usize, bool) {
    let mut ascii = true;
    for (i, &b) in input.iter().enumerate() {
        if b == b'"' || b == b'\\' || b < 0x20 {
            return (i, ascii);
        }
        ascii &= b < 0x80;
    }
    (input.len(), ascii)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn special_ascii_neon(input: &[u8]) -> (usize, bool) {
    use core::arch::aarch64::*;
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let space = vdupq_n_u8(b' ');
        let mut offset = 0;
        let mut ascii = true;
        while input.len() - offset >= 32 {
            let b0 = vld1q_u8(input.as_ptr().add(offset));
            let b1 = vld1q_u8(input.as_ptr().add(offset + 16));
            let bytes = b0;
            let mask = neon_special_mask(bytes, quote, slash, space);
            let first = (mask.trailing_zeros() / 4) as usize;
            let all_ascii = vmaxvq_u8(bytes) < 0x80;
            if first != 16 {
                if !all_ascii {
                    let high = vcgtq_u8(bytes, vdupq_n_u8(0x7f));
                    let high = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                        vreinterpretq_u16_u8(high),
                    )));
                    ascii &= high & ((1u64 << (first * 4)) - 1) == 0;
                }
                return (offset + 0 + first, ascii);
            }
            ascii &= all_ascii;
            let bytes = b1;
            let mask = neon_special_mask(bytes, quote, slash, space);
            let first = (mask.trailing_zeros() / 4) as usize;
            let all_ascii = vmaxvq_u8(bytes) < 0x80;
            if first != 16 {
                if !all_ascii {
                    let high = vcgtq_u8(bytes, vdupq_n_u8(0x7f));
                    let high = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                        vreinterpretq_u16_u8(high),
                    )));
                    ascii &= high & ((1u64 << (first * 4)) - 1) == 0;
                }
                return (offset + 16 + first, ascii);
            }
            ascii &= all_ascii;
            offset += 32;
        }
        if input.len() - offset >= 16 {
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            let mask = neon_special_mask(bytes, quote, slash, space);
            let first = (mask.trailing_zeros() / 4) as usize;
            let all_ascii = vmaxvq_u8(bytes) < 0x80;
            if first != 16 {
                if !all_ascii {
                    let high = vcgtq_u8(bytes, vdupq_n_u8(0x7f));
                    let high = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                        vreinterpretq_u16_u8(high),
                    )));
                    ascii &= high & ((1u64 << (first * 4)) - 1) == 0;
                }
                return (offset + 0 + first, ascii);
            }
            ascii &= all_ascii;
            offset += 16;
        }
        let (end, tail_ascii) = scalar_dom_ascii(&input[offset..]);
        (offset + end, ascii && tail_ascii)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn neon_special_mask(
    bytes: core::arch::aarch64::uint8x16_t,
    quote: core::arch::aarch64::uint8x16_t,
    slash: core::arch::aarch64::uint8x16_t,
    below_space: core::arch::aarch64::uint8x16_t,
) -> u64 {
    use core::arch::aarch64::*;
    unsafe {
        let special = vorrq_u8(
            vorrq_u8(vceqq_u8(bytes, quote), vceqq_u8(bytes, slash)),
            vcltq_u8(bytes, below_space),
        );
        let packed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(special));
        vget_lane_u64::<0>(vreinterpret_u64_u8(packed))
    }
}

#[inline(always)]
fn digit_run_end(input: &[u8], mut at: usize) -> usize {
    const LOW: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    while input.len() - at >= 8 {
        let word = u64::from_le_bytes(input[at..at + 8].try_into().unwrap_or_default());
        let mask = (word.wrapping_sub(LOW * 0x30) | word.wrapping_add(LOW * 0x46)) & HIGH;
        if mask != 0 {
            return at + (mask.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    while input.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    at
}
fn close<const FAST: bool>(
    cursor: &mut FastDomCursor<'_, FAST>,
    scratch: &mut DomScratch,
    shapes: &mut Shapes,
) -> Result<()> {
    let frame = scratch.stack.pop().ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            cursor.offset(),
            "DOM close has no open container",
        )
    })?;
    // The grammar loop called close only after observing this exact delimiter.
    cursor.advance(1);
    let end = tape_index(scratch.tape.len(), cursor.offset())?;
    let index = frame.node as usize;
    let parent = scratch.tape[index + 1].low32();
    scratch.tape[index + 1] = TapeWord::pack_pair(parent, end);
    scratch.tape[index + 2] = TapeWord::pack_pair(frame.count, cursor.offset());
    if frame.tag == TAG_OBJECT {
        if cursor.limits().reject_duplicate_keys {
            shape_close(shapes, scratch.stack.len());
        }
        if let Some(depth) = scratch.seen.get_mut(scratch.stack.len()) {
            depth.reset();
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct ShapeKey {
    scalar_len: u32,
    raw_token: Span32,
    hash: u64,
}
impl ShapeKey {
    const EMPTY: Self = Self {
        scalar_len: 0,
        raw_token: Span32::new(0, 0),
        hash: 0,
    };
}

const NO_MEMO: usize = usize::MAX;
const MAX_SHAPE_DEPTH: usize = 16;
const MEMO_SLOTS: usize = 64;
const MAX_SHAPE_KEYS: usize = 64;

pub(super) struct KeyShape {
    current: Vec<ShapeKey>,
    slot: usize,
    memo_start: usize,
    memo_len: usize,
    matches_previous: bool,
    disabled: bool,
    matched_keys: usize,
    table_seeded: bool,
}
impl Default for KeyShape {
    fn default() -> Self {
        Self {
            current: Vec::new(),
            slot: NO_MEMO,
            memo_start: 0,
            memo_len: 0,
            matches_previous: false,
            disabled: false,
            matched_keys: 0,
            table_seeded: false,
        }
    }
}
/// A memo owns `cap` entries of the shared arena starting at `start`.
struct ShapeMemo {
    depth: usize,
    first_hash: u64,
    start: u32,
    len: u32,
    cap: u32,
    used: bool,
}
impl Default for ShapeMemo {
    fn default() -> Self {
        Self {
            depth: NO_MEMO,
            first_hash: 0,
            start: 0,
            len: 0,
            cap: 0,
            used: false,
        }
    }
}
#[inline(always)]
fn memo_keys<'a>(arena: &'a [ShapeKey], memo: &ShapeMemo) -> &'a [ShapeKey] {
    arena
        .get(memo.start as usize..(memo.start + memo.len) as usize)
        .unwrap_or(&[])
}
fn grow_keys(keys: &mut Vec<ShapeKey>, additional: usize, offset: u32) -> Result<()> {
    keys.try_reserve(additional).map_err(|_| {
        dom_error(
            ErrorCode::BudgetExhausted,
            offset,
            "DOM shape storage allocation failed",
        )
    })
}
/// Separate first-key shapes at the same depth survive intervening objects.
/// Everything is grown on first use and lives in the reusable scratch, so a
/// parse that never repeats a shape pays no fixed initialization cost.
/// Memos are only valid within one build; `reset` drops them.
#[derive(Default)]
pub(super) struct Shapes {
    levels: Vec<KeyShape>,
    memos: Vec<ShapeMemo>,
    arena: Vec<ShapeKey>,
    touched: bool,
}
impl Shapes {
    fn reset(&mut self) {
        if self.touched {
            for memo in &mut self.memos {
                *memo = ShapeMemo::default();
            }
            self.arena.clear();
            self.touched = false;
        }
    }
}
impl core::fmt::Debug for Shapes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shapes").finish_non_exhaustive()
    }
}
impl core::ops::Deref for Shapes {
    type Target = [KeyShape];
    fn deref(&self) -> &[KeyShape] {
        &self.levels
    }
}
impl core::ops::DerefMut for Shapes {
    fn deref_mut(&mut self) -> &mut [KeyShape] {
        &mut self.levels
    }
}
fn memo_slot(depth: usize, hash: u64) -> usize {
    ((hash ^ (depth as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)) >> 58) as usize
}
#[inline(always)]
fn shape_open(shapes: &mut Shapes, depth: usize, offset: u32) -> Result<()> {
    if depth >= MAX_SHAPE_DEPTH {
        return Ok(());
    }
    if shapes.levels.len() <= depth {
        grow_levels(shapes, offset)?;
    }
    let shape = &mut shapes.levels[depth];
    shape.current.clear();
    shape.slot = NO_MEMO;
    shape.matches_previous = false;
    shape.disabled = false;
    shape.matched_keys = 0;
    shape.table_seeded = false;
    Ok(())
}
#[cold]
#[inline(never)]
fn grow_levels(shapes: &mut Shapes, offset: u32) -> Result<()> {
    reserve(&mut shapes.levels, MAX_SHAPE_DEPTH, offset)?;
    shapes
        .levels
        .resize_with(MAX_SHAPE_DEPTH, KeyShape::default);
    Ok(())
}

#[inline(always)]
fn shape_token_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    if (8..=16).contains(&n) {
        return load_u64(a, 0) == load_u64(b, 0) && load_u64(a, n - 8) == load_u64(b, n - 8);
    }
    if (4..8).contains(&n) {
        return load_u32(a, 0) == load_u32(b, 0) && load_u32(a, n - 4) == load_u32(b, n - 4);
    }
    a == b
}

#[inline(always)]
fn match_shape_token(
    shapes: &mut Shapes,
    depth: usize,
    raw: &[u8],
    offset: u32,
    limits: Limits,
) -> Option<StringMeta> {
    let shape = shapes.levels.get_mut(depth)?;
    if shape.disabled || !shape.matches_previous {
        return None;
    }
    if shape.matched_keys >= shape.memo_len {
        return None;
    }
    let key = *shapes.arena.get(shape.memo_start + shape.matched_keys)?;
    if key.raw_token.len - 2 > limits.max_string_bytes {
        return None;
    }
    let token = Span32::new(offset, key.raw_token.len);
    if !shape_token_eq(token.slice(raw)?, key.raw_token.slice(raw)?) {
        return None;
    }
    shape.matched_keys += 1;
    let content = Span32::new(offset + 1, key.raw_token.len - 2);
    Some(StringMeta {
        raw_content: content,
        raw_token: token,
        cooked: StrRef {
            span: content,
            source: SpanSource::RawPayload,
            scalar_len: key.scalar_len,
        },
        is_unescaped: true,
        is_ascii: key.scalar_len == content.len,
    })
}

fn shape_hash(
    shapes: &mut Shapes,
    depth: usize,
    raw: &[u8],
    decoded: &[u8],
    reference: StrRef,
    raw_token: Span32,
    _ascii: bool,
    offset: u32,
) -> Result<(u64, bool, usize)> {
    let shape = shapes.levels.get_mut(depth).ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            offset,
            "object key shape is not open",
        )
    })?;
    let hash = key_hash(string_bytes(raw, decoded, reference.span, reference.source));
    let prefix = shape.matched_keys;
    if !shape.matches_previous
        && shape.current.is_empty()
        && prefix == 0
        && reference.source == SpanSource::RawPayload
    {
        let slot = memo_slot(depth, hash);
        if let Some(memo) = shapes.memos.get_mut(slot)
            && let Some(first) = memo_keys(&shapes.arena, memo).first().copied()
            && memo.depth == depth
            && memo.first_hash == hash
            && raw_token
                .slice(raw)
                .zip(first.raw_token.slice(raw))
                .is_some_and(|(a, b)| shape_token_eq(a, b))
        {
            shape.slot = slot;
            shape.memo_start = memo.start as usize;
            shape.memo_len = memo.len as usize;
            shape.matches_previous = true;
            shape.matched_keys = 1;
            memo.used = true;
            shapes.touched = true;
            return Ok((first.hash, true, 0));
        }
    }
    if shape.matches_previous {
        let memo_keys = shapes
            .arena
            .get(shape.memo_start..shape.memo_start + shape.memo_len)
            .unwrap_or(&[]);
        if let Some(key) = memo_keys.get(prefix) {
            if reference.source == SpanSource::RawPayload
                && raw_token
                    .slice(raw)
                    .zip(key.raw_token.slice(raw))
                    .is_some_and(|(a, b)| shape_token_eq(a, b))
            {
                shape.matched_keys += 1;
                return Ok((key.hash, true, prefix));
            }
        }
        shape.matches_previous = false;
        grow_keys(&mut shape.current, prefix.max(16), offset)?;
        shape.current.extend_from_slice(&memo_keys[..prefix]);
    }
    if shape.current.len() >= MAX_SHAPE_KEYS || reference.source != SpanSource::RawPayload {
        shape.disabled = true;
    }
    if !shape.disabled {
        if shape.current.len() == shape.current.capacity() {
            grow_keys(&mut shape.current, 16, offset)?;
        }
        shape.current.push(ShapeKey {
            scalar_len: reference.scalar_len,
            raw_token,
            hash,
        });
    }
    Ok((hash, false, prefix))
}

fn seed_shape_prefix(
    scratch: &mut DomScratch,
    shapes: &Shapes,
    raw: &[u8],
    depth: usize,
    prefix_len: usize,
    offset: u32,
) -> Result<()> {
    let shape = shapes.levels.get(depth).ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            offset,
            "object key shape is not open",
        )
    })?;
    for key in shape.current.iter().take(prefix_len) {
        check_duplicate(
            scratch,
            raw,
            depth,
            key.hash,
            StrRef {
                span: Span32::new(key.raw_token.offset + 1, key.raw_token.len - 2),
                source: SpanSource::RawPayload,
                scalar_len: key.scalar_len,
            },
            offset,
        )?;
    }
    Ok(())
}

fn shape_close(shapes: &mut Shapes, depth: usize) {
    let Some(shape) = shapes.levels.get_mut(depth) else {
        return;
    };
    if shape.slot != NO_MEMO {
        shapes.memos[shape.slot].used = false;
    }
    if !shape.disabled && !shape.matches_previous && !shape.current.is_empty() {
        // The memo is only a cache: a failed allocation just skips storing it.
        if shapes.memos.is_empty() {
            if shapes.memos.try_reserve_exact(MEMO_SLOTS).is_err() {
                return;
            }
            shapes.memos.resize_with(MEMO_SLOTS, ShapeMemo::default);
        }
        let hash = shape.current[0].hash;
        let memo = &mut shapes.memos[memo_slot(depth, hash)];
        // A descendant cannot replace the memo an open ancestor is matching.
        if !memo.used {
            let n = shape.current.len();
            if n > memo.cap as usize {
                let cap = n.next_power_of_two().max(8);
                let start = shapes.arena.len();
                if start + cap > u32::MAX as usize || shapes.arena.try_reserve(cap).is_err() {
                    return;
                }
                shapes.arena.extend_from_slice(&shape.current);
                shapes.arena.resize(start + cap, ShapeKey::EMPTY);
                memo.start = start as u32;
                memo.cap = cap as u32;
            } else {
                let start = memo.start as usize;
                shapes.arena[start..start + n].copy_from_slice(&shape.current);
            }
            memo.len = n as u32;
            memo.depth = depth;
            memo.first_hash = hash;
            shapes.touched = true;
        }
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;
    #[test]
    fn interleaved_shape_cache_cannot_replace_an_open_ancestor() {
        let parent_slot = memo_slot(1, key_hash(b"a"));
        let child = (0..10_000)
            .map(|i| alloc::format!("child_{i}"))
            .find(|key| memo_slot(2, key_hash(key.as_bytes())) == parent_slot)
            .unwrap();
        let rows = (0..32)
            .map(|i| alloc::format!("{{\"a\":{i},\"child\":{{\"{child}\":{i}}},\"last\":{i}}}"))
            .collect::<Vec<_>>()
            .join(",");
        let valid = alloc::format!("[{rows}]");
        let invalid = valid.replacen("\"last\":31", "\"a\":31", 1);
        for raw in [valid, invalid] {
            let mut direct = DomScratch::default();
            let mut reference = DomScratch::default();
            let actual = build_inner(raw.as_bytes(), Limits::strict_default(), None, &mut direct);
            let expected = super::super::build_inner(
                raw.as_bytes(),
                Limits::strict_default(),
                None,
                &mut reference,
            );
            match (actual, expected) {
                (Ok(_), Ok(_)) => {
                    assert_eq!(direct.tape, reference.tape);
                    assert_eq!(direct.decoded, reference.decoded);
                }
                (Err(a), Err(b)) => assert_eq!(a, b),
                _ => panic!("ancestor memo replacement changed acceptance"),
            }
        }
    }

    #[test]
    fn reused_scratch_drops_memos_between_documents() {
        let a = r#"[{"a":0,"b":1},{"a":2,"b":3}]"#;
        let b = r#"[{"x":0,"a":1},{"x":2,"a":3,"b":9},{"a":4,"b":5}]"#;
        let c = r#"[{"a":0,"b":1},{"a":2,"b":3,"a":4}]"#;
        let mut shared = DomScratch::default();
        for raw in [a, b, a, c, b] {
            let mut fresh = DomScratch::default();
            let x = build_inner(raw.as_bytes(), Limits::strict_default(), None, &mut shared);
            let y = build_inner(raw.as_bytes(), Limits::strict_default(), None, &mut fresh);
            match (x, y) {
                (Ok(_), Ok(_)) => assert_eq!(shared.tape, fresh.tape),
                (Err(x), Err(y)) => assert_eq!((x.code, x.byte_offset), (y.code, y.byte_offset)),
                _ => panic!("reuse changed acceptance: {raw}"),
            }
            shared.clear_keep_capacity();
        }
    }

    #[test]
    fn string_candidate_keeps_non_ascii_prefix_state() {
        assert_eq!(
            string_special_ascii("aaaaaaaaaaaaaé\"".as_bytes()),
            (15, false)
        );
        assert_eq!(string_special_ascii("aaaaaé\"".as_bytes()), (7, false));
        assert_eq!(string_special_ascii(b"abc\\x"), (3, true));
    }

    #[test]
    fn memo_divergence_and_cap_match_reference_tape_and_errors() {
        let cases = [
            alloc::string::String::from(r#"[{"a":0,"b":1},{"a":2,"b":3,"a":4}]"#),
            alloc::string::String::from(r#"[{"a":0,"b":1},{"\u0061":2,"b":3,"a":4}]"#),
            alloc::string::String::from(r#"[{"\u0061":0,"b":1},{"\u0061":2,"b":3}]"#),
            {
                let keys = (0..40)
                    .map(|n| alloc::format!("\"k{n}\":{n}"))
                    .collect::<Vec<_>>()
                    .join(",");
                alloc::format!("[{{{keys}}},{{{keys},\"k0\":99}}]")
            },
        ];
        for raw in cases {
            let mut fast = DomScratch::default();
            let mut reference = DomScratch::default();
            let a = build_inner(raw.as_bytes(), Limits::strict_default(), None, &mut fast);
            let b = super::super::build_inner(
                raw.as_bytes(),
                Limits::strict_default(),
                None,
                &mut reference,
            );
            match (a, b) {
                (Ok(_), Ok(_)) => {
                    assert_eq!(fast.tape, reference.tape);
                    assert_eq!(fast.decoded, reference.decoded);
                }
                (Err(a), Err(b)) => {
                    assert_eq!(a.code, b.code);
                    assert_eq!(a.byte_offset, b.byte_offset);
                    assert_eq!(a.source, b.source);
                }
                _ => panic!("fast and reference disagree: {raw}"),
            }
        }
    }
}

#[inline(always)]
fn skip_ws(input: &[u8], mut at: usize) -> usize {
    while input
        .get(at)
        .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        at += 1;
    }
    at
}
