//! One-pass conservative strict acceptor.
//!
//! `strict_discard_loop` is the authority for what strict skipping accepts, with
//! exact error codes and offsets. This module is only a faster way to answer
//! "does it accept?" for the common clean input. It returns `Some` only when it
//! has proven that the exact loop would accept the same bytes, with the same end
//! offset, the same discarded-decoded total, and the same final `decoded`
//! contents and capacity. On any error, limit, rare feature or doubt it returns
//! `None` and restores `decoded` (length and capacity), so the caller then runs
//! the exact loop from untouched state and produces every diagnostic.
//!
//! The acceptor mirrors the exact loop's resource accounting rather than
//! re-deriving it: escaped strings go through the same `decode_content` with
//! the same adjusted limits, keys are captured into `decoded` exactly when
//! duplicate rejection is on, and a closing container reclaims its keys into
//! the discarded total just as `strict_discard_loop` does.

use super::block_cursor::decode_content;
use super::frames::KeyTable;
use super::*;
use jmax_abi::number::NumericPolicy;

/// Keys compared linearly by fingerprint before a container switches to a table.
const INLINE_KEYS: usize = 32;
/// A float without an exponent and at most this many bytes cannot overflow or
/// underflow under any numeric policy, so the policy check is a no-op for it.
const POLICY_SAFE_FLOAT_BYTES: usize = 300;

#[derive(Clone, Copy, Debug)]
struct Level {
    count: u32,
    /// Start of this container's keys in `FastScratch::keys` (objects only).
    seg_start: u32,
    /// `decoded.len()` when the container opened.
    checkpoint: u32,
    object: bool,
    /// Keys live in `tables[depth]` instead of the inline segment.
    table: bool,
    /// Some key of this object was escaped, so its shape is not memoizable.
    escaped: bool,
    /// Slot of the shape memo whose keys every key so far has matched, or `NO_SHAPE`.
    /// While set, only key 0 is materialized in the inline segment.
    shape: u32,
}

const NO_SHAPE: u32 = u32::MAX;
const SHAPE_SLOTS: usize = 32;
/// Longest key list a shape memo (and a table-mode segment) keeps.
const SHAPE_MAX_KEYS: usize = 256;

#[derive(Clone, Copy, Debug)]
struct ShapeKey {
    fingerprint: u64,
    offset: u32,
    len: u32,
}

/// The keys of the last closed object at a depth with this first key: all
/// unescaped, pairwise distinct, spelled at `offset` in the current input.
#[derive(Debug, Default)]
struct Shape {
    generation: u32,
    depth: u32,
    keys: Vec<ShapeKey>,
}

#[derive(Clone, Copy, Debug)]
struct KeyEnt {
    fingerprint: u64,
    offset: u32,
    len: u32,
    decoded: bool,
}

/// Reusable storage; nothing here retains input or decoded bytes.
/// Memoized key shapes (see `Shape`) persist across `accept` calls, so every
/// owner must call `FastScratch::new_input` before the first call that reads
/// a different (or possibly rewritten) input slice.
#[derive(Debug, Default)]
pub(crate) struct FastScratch {
    levels: Vec<Level>,
    keys: Vec<KeyEnt>,
    tables: Vec<KeyTable>,
    shapes: Vec<Shape>,
    /// First objects at each shallow depth only mark a bit. Memo lookup and
    /// recording start with the second object; deeper objects use exact tables.
    seen_objects: u64,
    generation: u32,
}

impl FastScratch {
    /// Forgets every memoized shape. Call whenever the input slice may differ
    /// from the one previous `accept` calls saw.
    pub(crate) fn new_input(&mut self) {
        self.seen_objects = 0;
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            for shape in &mut self.shapes {
                shape.generation = 0;
            }
            self.generation = 1;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Params {
    pub limits: Limits,
    pub numeric: Option<NumericPolicy>,
    /// Containers that may still open below the value (`max_depth - current_depth`).
    pub max_depth: usize,
    pub masker: jmax_string::EscapeMasker,
    /// The value must be followed only by whitespace to the end of the input.
    pub whole_input: bool,
}

#[derive(Clone, Copy)]
struct Cooked {
    decoded: bool,
    offset: u32,
    len: u32,
}

/// Tries to strictly validate the value starting at `start` (a non-whitespace
/// byte, or whitespace for `whole_input`). Returns the end offset of the value
/// (of the trailing whitespace when `whole_input`) and the cumulative
/// discarded-decoded total after it.
pub(crate) fn accept(
    input: &[u8],
    start: usize,
    params: &Params,
    discarded: u32,
    decoded: &mut Vec<u8>,
    scratch: &mut FastScratch,
) -> Option<(usize, u32)> {
    let len0 = decoded.len();
    let cap0 = decoded.capacity();
    scratch.levels.clear();
    scratch.keys.clear();
    let mut ctx = Ctx {
        input,
        p: params,
        decoded,
        s: scratch,
        discarded,
        // Shape bookkeeping costs more than it can save on a short remaining
        // slice (notably a lone object). Keep the ordinary path untouched there.
        memo_enabled: params.limits.reject_duplicate_keys && start <= input.len() && input.len() - start >= 256,
    };
    let result = ctx.run(start);
    let Ctx { decoded, .. } = ctx;
    if result.is_none() {
        decoded.truncate(len0);
        if decoded.capacity() != cap0 {
            // Only growth can have happened. The exact loop must start from
            // the capacity the caller handed over.
            decoded.shrink_to(cap0);
        }
    }
    result
}

struct Ctx<'a> {
    input: &'a [u8],
    p: &'a Params,
    decoded: &'a mut Vec<u8>,
    s: &'a mut FastScratch,
    discarded: u32,
    memo_enabled: bool,
}

impl Ctx<'_> {
    fn run(&mut self, start: usize) -> Option<(usize, u32)> {
        let input = self.input;
        let mut i = start;
        loop {
            i = skip_ws(input, i);
            let byte = *input.get(i)?;
            match byte {
                b'"' => i = self.string(i, false)?.0,
                b'{' => {
                    self.open(true)?;
                    i = skip_ws(input, i + 1);
                    match *input.get(i)? {
                        b'}' => {
                            i += 1;
                            self.close()?;
                        }
                        b'"' => {
                            i = self.key(i)?;
                            continue;
                        }
                        _ => return None,
                    }
                }
                b'[' => {
                    self.open(false)?;
                    i = skip_ws(input, i + 1);
                    if *input.get(i)? == b']' {
                        i += 1;
                        self.close()?;
                    } else {
                        self.count_item()?;
                        continue;
                    }
                }
                b'-' | b'0'..=b'9' => i = self.number(i)?,
                b't' => i = literal(input, i, b"true")?,
                b'f' => i = literal(input, i, b"false")?,
                b'n' => i = literal(input, i, b"null")?,
                _ => return None,
            }
            // A value ended at `i`; close containers and find the next value.
            loop {
                let Some(top) = self.s.levels.last() else {
                    if self.p.whole_input {
                        i = skip_ws(input, i);
                        if i != input.len() {
                            return None;
                        }
                    }
                    return Some((i, self.discarded));
                };
                let object = top.object;
                i = skip_ws(input, i);
                let byte = *input.get(i)?;
                if object {
                    match byte {
                        b',' => {
                            i = skip_ws(input, i + 1);
                            if *input.get(i)? != b'"' {
                                return None;
                            }
                            i = self.key(i)?;
                            break;
                        }
                        b'}' => {
                            i += 1;
                            self.close()?;
                        }
                        _ => return None,
                    }
                } else {
                    match byte {
                        b',' => {
                            i += 1;
                            self.count_item()?;
                            break;
                        }
                        b']' => {
                            i += 1;
                            self.close()?;
                        }
                        _ => return None,
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn open(&mut self, object: bool) -> Option<()> {
        if self.s.levels.len() >= self.p.max_depth {
            return None;
        }
        if self.s.levels.len() == self.s.levels.capacity() {
            self.s.levels.try_reserve(1).ok()?;
        }
        let depth = self.s.levels.len();
        let memo = if self.memo_enabled && object && depth < u64::BITS as usize {
            let bit = 1u64 << depth;
            let repeated = self.s.seen_objects & bit != 0;
            self.s.seen_objects |= bit;
            repeated
        } else { false };
        self.s.levels.push(Level {
            count: 0,
            seg_start: u32::try_from(self.s.keys.len()).ok()?,
            checkpoint: u32::try_from(self.decoded.len()).ok()?,
            object,
            table: false,
            escaped: !memo,
            shape: NO_SHAPE,
        });
        Some(())
    }

    /// The exact loop's reclaim at a container close: keys captured inside it
    /// leave the arena and join the discarded total.
    #[inline(always)]
    fn close(&mut self) -> Option<()> {
        let level = self.s.levels.pop()?;
        if self.memo_enabled
            && level.object
            && level.shape == NO_SHAPE
            && !level.escaped
            && level.count >= 2
        {
            self.record_shape(&level);
        }
        let checkpoint = level.checkpoint as usize;
        if self.decoded.len() > checkpoint {
            self.discarded = self
                .discarded
                .saturating_add((self.decoded.len() - checkpoint) as u32);
            self.decoded.truncate(checkpoint);
        }
        self.s.keys.truncate(level.seg_start as usize);
        Some(())
    }

    #[inline(always)]
    fn count_item(&mut self) -> Option<()> {
        let level = self.s.levels.last_mut()?;
        let next = level.count.checked_add(1)?;
        if next > self.p.limits.max_array_items {
            return None;
        }
        level.count = next;
        Some(())
    }

    /// A key at `i` (its opening quote), the field count and duplicate checks,
    /// and the colon. Returns the offset after the colon.
    #[inline(always)]
    fn key(&mut self, i: usize) -> Option<usize> {
        let reject_dup = self.p.limits.reject_duplicate_keys;
        let top = self.s.levels.len().checked_sub(1)?;
        if self.memo_enabled && reject_dup && self.s.levels[top].shape != NO_SHAPE {
            if let Some(next) = self.shape_key(i, top)? {
                return self.colon(next);
            }
        }
        let (next, cooked) = self.string(i, reject_dup)?;
        {
            let level = self.s.levels.last_mut()?;
            let count = level.count.checked_add(1)?;
            if count > self.p.limits.max_fields_per_object {
                return None;
            }
            level.count = count;
        }
        if reject_dup {
            self.check_unique(cooked)?;
            if self.s.levels[top].escaped {
                // First objects and short slices use the ordinary duplicate path.
            } else if cooked.decoded {
                self.s.levels[top].escaped = true;
            } else if self.s.levels[top].count == 1 && !self.s.shapes.is_empty() {
                self.adopt_shape(top);
            }
        }
        self.colon(next)
    }

    #[inline(always)]
    fn colon(&self, next: usize) -> Option<usize> {
        let j = skip_ws(self.input, next);
        if *self.input.get(j)? != b':' {
            return None;
        }
        Some(j + 1)
    }

    /// Tries the memoized shape of this object for the key at `i`. `Some(Some(next))`:
    /// the key's raw bytes equal the memo key's, so it is the same unescaped,
    /// valid, distinct key (counted, offset after its closing quote).
    /// `Some(None)`: no match; the memo keys are materialized and the caller
    /// takes the ordinary path. `None`: decline.
    #[inline(always)]
    fn shape_key(&mut self, i: usize, top: usize) -> Option<Option<usize>> {
        let level = self.s.levels[top];
        let input = self.input;
        let content = i + 1;
        let hit = match self.s.shapes.get(level.shape as usize)?.keys.get(level.count as usize) {
            Some(key) => {
                let n = key.len as usize + 1;
                let at = key.offset as usize;
                match (input.get(content..content + n), input.get(at..at + n)) {
                    (Some(a), Some(b)) if bytes_eq(a, b) => Some(content + n),
                    _ => None,
                }
            }
            None => None,
        };
        if let Some(next) = hit {
            let count = level.count.checked_add(1)?;
            if count > self.p.limits.max_fields_per_object {
                return None;
            }
            self.s.levels[top].count = count;
            return Some(Some(next));
        }
        // Materialize memo keys 1..count (key 0 is already in the segment).
        let FastScratch { keys, shapes, .. } = &mut *self.s;
        let shape = shapes.get(level.shape as usize)?;
        for key in shape.keys.get(1..level.count as usize)? {
            keys.try_reserve(1).ok()?;
            keys.push(KeyEnt {
                fingerprint: key.fingerprint,
                offset: key.offset,
                len: key.len,
                decoded: false,
            });
        }
        self.s.levels[top].shape = NO_SHAPE;
        Some(None)
    }

    /// After the first key of an object: follow a memoized shape if one starts
    /// with this exact key at this depth.
    #[inline(always)]
    fn adopt_shape(&mut self, top: usize) {
        let level = self.s.levels[top];
        if self.s.shapes.is_empty() || level.table {
            return;
        }
        let Some(first) = self.s.keys.get(level.seg_start as usize) else {
            return;
        };
        let slot = shape_slot(top, first.fingerprint);
        let shape = &self.s.shapes[slot];
        if shape.generation != self.s.generation || shape.depth as usize != top {
            return;
        }
        let Some(key) = shape.keys.first() else {
            return;
        };
        if key.fingerprint != first.fingerprint || key.len != first.len {
            return;
        }
        let n = key.len as usize;
        let (a, b) = (key.offset as usize, first.offset as usize);
        if self.input.get(a..a + n) == self.input.get(b..b + n) {
            self.s.levels[top].shape = slot as u32;
        }
    }

    /// Remembers the keys of a closing object (all unescaped, distinct, inline).
    fn record_shape(&mut self, level: &Level) {
        let depth = self.s.levels.len();
        let slot = match self.s.keys.get(level.seg_start as usize..) {
            Some(segment) if segment.len() == level.count as usize => {
                shape_slot(depth, segment[0].fingerprint)
            }
            _ => return,
        };
        // An open object may still be matching against this very slot.
        if self.s.levels.iter().any(|l| l.shape == slot as u32) {
            return;
        }
        if self.s.shapes.len() < SHAPE_SLOTS {
            if self.s.shapes.try_reserve(SHAPE_SLOTS).is_err() {
                return;
            }
            self.s.shapes.resize_with(SHAPE_SLOTS, Shape::default);
        }
        let FastScratch { keys, shapes, generation, .. } = &mut *self.s;
        let segment = &keys[level.seg_start as usize..];
        let shape = &mut shapes[slot];
        shape.keys.clear();
        if shape.keys.try_reserve(segment.len()).is_err() {
            shape.generation = 0;
            return;
        }
        shape.generation = *generation;
        shape.depth = depth as u32;
        shape.keys.extend(segment.iter().map(|e| ShapeKey {
            fingerprint: e.fingerprint,
            offset: e.offset,
            len: e.len,
        }));
    }

    fn check_unique(&mut self, cooked: Cooked) -> Option<()> {
        let input = self.input;
        let start = cooked.offset as usize;
        let end = start.checked_add(cooked.len as usize)?;
        let owner: &[u8] = if cooked.decoded { self.decoded } else { input };
        let bytes = owner.get(start..end)?;
        let fingerprint = fingerprint(bytes);
        let depth = self.s.levels.len().checked_sub(1)?;
        let level = self.s.levels[depth];
        let reference = StrRef {
            span: Span32::new(cooked.offset, cooked.len),
            source: if cooked.decoded {
                SpanSource::DecodedArena
            } else {
                SpanSource::RawPayload
            },
            scalar_len: 0,
        };
        if level.table {
            let table = self.s.tables.get_mut(depth)?;
            let duplicate = table
                .insert_unique(fingerprint, reference, bytes, input, self.decoded, 0)
                .ok()?;
            if duplicate {
                return None;
            }
            return self.remember(depth, cooked, fingerprint);
        }
        let segment = self.s.keys.get(level.seg_start as usize..)?;
        for entry in segment {
            if entry.fingerprint != fingerprint || entry.len != cooked.len {
                continue;
            }
            let other_owner: &[u8] = if entry.decoded { self.decoded } else { input };
            let other = other_owner
                .get(entry.offset as usize..(entry.offset as usize).checked_add(entry.len as usize)?)?;
            if other == bytes {
                return None;
            }
        }
        if segment.len() < INLINE_KEYS {
            if self.s.keys.len() == self.s.keys.capacity() {
                self.s.keys.try_reserve(1).ok()?;
            }
            self.s.keys.push(KeyEnt {
                fingerprint,
                offset: cooked.offset,
                len: cooked.len,
                decoded: cooked.decoded,
            });
            return Some(());
        }
        // Too many keys for a linear scan: move this container to a table.
        while self.s.tables.len() <= depth {
            self.s.tables.try_reserve(1).ok()?;
            self.s.tables.push(KeyTable::new());
        }
        let FastScratch { keys, tables, .. } = &mut *self.s;
        let table = &mut tables[depth];
        table.reset();
        for entry in &keys[level.seg_start as usize..] {
            let owner: &[u8] = if entry.decoded { self.decoded } else { input };
            let existing = owner
                .get(entry.offset as usize..(entry.offset as usize).checked_add(entry.len as usize)?)?;
            let existing_ref = StrRef {
                span: Span32::new(entry.offset, entry.len),
                source: if entry.decoded {
                    SpanSource::DecodedArena
                } else {
                    SpanSource::RawPayload
                },
                scalar_len: 0,
            };
            if table
                .insert_unique(entry.fingerprint, existing_ref, existing, input, self.decoded, 0)
                .ok()?
            {
                return None;
            }
        }
        if table
            .insert_unique(fingerprint, reference, bytes, input, self.decoded, 0)
            .ok()?
        {
            return None;
        }
        self.s.levels[depth].table = true;
        self.remember(depth, cooked, fingerprint)
    }

    /// Table-mode objects keep their key list in the segment too (up to
    /// `SHAPE_MAX_KEYS`), so that their shape can be memoized.
    #[inline(always)]
    fn remember(&mut self, depth: usize, cooked: Cooked, fingerprint: u64) -> Option<()> {
        let level = self.s.levels[depth];
        if level.escaped {
            self.s.keys.truncate(level.seg_start as usize);
            return Some(());
        }
        if self.s.keys.len() - (level.seg_start as usize) >= SHAPE_MAX_KEYS {
            self.s.levels[depth].escaped = true;
            return Some(());
        }
        if self.s.keys.len() == self.s.keys.capacity() {
            self.s.keys.try_reserve(1).ok()?;
        }
        self.s.keys.push(KeyEnt {
            fingerprint,
            offset: cooked.offset,
            len: cooked.len,
            decoded: cooked.decoded,
        });
        Some(())
    }

    /// A string whose opening quote is at `i`. `capture` is true exactly when
    /// the exact loop captures the cooked bytes (keys under duplicate rejection).
    #[inline(always)]
    fn string(&mut self, i: usize, capture: bool) -> Option<(usize, Cooked)> {
        let input = self.input;
        let content = i + 1;
        let mut j = find_special(input, content);
        let byte = *input.get(j)?;
        if byte == b'"' {
            let len = j - content;
            if len > self.p.limits.max_string_bytes as usize {
                return None;
            }
            let raw = &input[content..j];
            if !raw.is_ascii() {
                jmax_utf8::validate_and_count_scalars(raw, content as u32).ok()?;
            }
            return Some((
                j + 1,
                Cooked {
                    decoded: false,
                    offset: content as u32,
                    len: len as u32,
                },
            ));
        }
        if byte < 0x20 {
            return None;
        }
        // A backslash: find the closing quote, skipping each escaped byte.
        loop {
            j += 2;
            if j >= input.len() {
                return None;
            }
            j = find_special(input, j);
            let byte = *input.get(j)?;
            if byte == b'"' {
                break;
            }
            if byte < 0x20 {
                return None;
            }
        }
        let mut limits = self.p.limits;
        limits.max_decoded_bytes = limits.max_decoded_bytes.saturating_sub(self.discarded);
        let (cooked, _) = decode_content(
            input,
            i as u32,
            j as u32,
            true,
            limits,
            self.decoded,
            capture,
            self.p.masker,
        )
        .ok()?;
        if !capture {
            self.discarded = self.discarded.saturating_add(cooked.span.len);
        }
        Some((
            j + 1,
            Cooked {
                decoded: true,
                offset: cooked.span.offset,
                len: cooked.span.len,
            },
        ))
    }

    /// JSON number grammar over the bytes at `i`, digit limit, boundary, and
    /// the numeric policy, deciding exactly the accepting cases of `number_inner`.
    #[inline(always)]
    fn number(&mut self, i: usize) -> Option<usize> {
        let input = self.input;
        let n = input.len();
        let mut j = i;
        let negative = input[j] == b'-';
        if negative {
            j += 1;
        }
        let first = *input.get(j)?;
        let int_start = j;
        match first {
            b'0' => j += 1,
            b'1'..=b'9' => {
                j += 1;
                while j < n && input[j].is_ascii_digit() {
                    j += 1;
                }
            }
            _ => return None,
        }
        let mut digits = j - int_start;
        let mut float = false;
        let mut exponent = false;
        if j < n && input[j] == b'.' {
            float = true;
            j += 1;
            let fraction = j;
            while j < n && input[j].is_ascii_digit() {
                j += 1;
            }
            if j == fraction {
                return None;
            }
            digits += j - fraction;
        }
        if j < n && (input[j] | 0x20) == b'e' {
            float = true;
            exponent = true;
            j += 1;
            if j < n && (input[j] == b'+' || input[j] == b'-') {
                j += 1;
            }
            let start = j;
            while j < n && input[j].is_ascii_digit() {
                j += 1;
            }
            if j == start || j - start > 12 {
                return None;
            }
            digits += j - start;
        }
        if j < n && !jmax_number::is_number_delimiter(input[j]) {
            return None;
        }
        if digits > usize::from(self.p.limits.max_number_digits) {
            return None;
        }
        let negative_zero = negative && first == b'0';
        if self.p.numeric.is_some() && (float || negative_zero) {
            let exact = exponent || negative_zero || j - i > POLICY_SAFE_FLOAT_BYTES;
            if exact && !super::feed::number_accepted_fast(&input[i..j], self.p.numeric) {
                return None;
            }
        }
        Some(j)
    }
}

#[inline(always)]
fn literal(input: &[u8], i: usize, spelling: &[u8]) -> Option<usize> {
    let end = i + spelling.len();
    if input.get(i..end)? != spelling {
        return None;
    }
    match input.get(end) {
        Some(&byte) if !jmax_number::is_number_delimiter(byte) => None,
        _ => Some(end),
    }
}

/// Equality of two slices of the same length, inline for short ones.
#[inline(always)]
fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    if n >= 8 && n <= 16 {
        let rd = |s: &[u8], at: usize| u64::from_le_bytes(s[at..at + 8].try_into().unwrap_or_default());
        return rd(a, 0) == rd(b, 0) && rd(a, n - 8) == rd(b, n - 8);
    }
    if n >= 4 && n < 8 {
        let rd = |s: &[u8], at: usize| u32::from_le_bytes(s[at..at + 4].try_into().unwrap_or_default());
        return rd(a, 0) == rd(b, 0) && rd(a, n - 4) == rd(b, n - 4);
    }
    a == b
}

/// Direct-mapped slot for the shape of an object at `depth` starting with a key of this fingerprint.
#[inline(always)]
fn shape_slot(depth: usize, fingerprint: u64) -> usize {
    let mixed = fingerprint ^ (depth as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed >> 59) as usize
}

const LOW_ONES: u64 = 0x0101_0101_0101_0101;
const HIGH_BITS: u64 = 0x8080_8080_8080_8080;

#[inline(always)]
fn load_word(input: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(input[i..i + 8].try_into().unwrap_or_default())
}

/// Whitespace, with whole words of spaces (indentation) skipped at a time.
#[inline(always)]
fn skip_ws(input: &[u8], mut i: usize) -> usize {
    while let Some(&byte) = input.get(i) {
        if byte == b' ' || byte == b'\n' || byte == b'\r' || byte == b'\t' {
            i += 1;
            while i + 8 <= input.len() && load_word(input, i) == LOW_ONES * 0x20 {
                i += 8;
            }
        } else {
            break;
        }
    }
    i
}

/// Index of the first byte at or after `i` that is `"`, `\` or below 0x20, or
/// the input length. Each term is exact for the lowest flagged byte; bytes above
/// a true match may be flagged falsely, which the lowest-bit extraction ignores.
#[inline(always)]
fn find_special(input: &[u8], mut i: usize) -> usize {
    let n = input.len();
    while i + 8 <= n {
        let word = load_word(input, i);
        let quote = word ^ (LOW_ONES * u64::from(b'"'));
        let slash = word ^ (LOW_ONES * u64::from(b'\\'));
        let found = (quote.wrapping_sub(LOW_ONES) & !quote)
            | (slash.wrapping_sub(LOW_ONES) & !slash)
            | (word.wrapping_sub(LOW_ONES * 0x20) & !word);
        let found = found & HIGH_BITS;
        if found != 0 {
            return i + (found.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n {
        let byte = input[i];
        if byte == b'"' || byte == b'\\' || byte < 0x20 {
            return i;
        }
        i += 1;
    }
    n
}

/// Length, head and tail words of the cooked key. Equal fingerprints are always
/// confirmed by comparing the bytes, so this only has to separate distinct keys.
#[inline(always)]
fn fingerprint(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let n = bytes.len();
    let (head, tail) = if n >= 8 {
        (
            u64::from_le_bytes(bytes[..8].try_into().unwrap_or_default()),
            u64::from_le_bytes(bytes[n - 8..].try_into().unwrap_or_default()),
        )
    } else if n >= 4 {
        let low = u32::from_le_bytes(bytes[..4].try_into().unwrap_or_default());
        let high = u32::from_le_bytes(bytes[n - 4..].try_into().unwrap_or_default());
        (u64::from(low), u64::from(high))
    } else if n > 0 {
        let word = u64::from(bytes[0]) | u64::from(bytes[n / 2]) << 8 | u64::from(bytes[n - 1]) << 16;
        (word, word)
    } else {
        (0, 0)
    };
    let mut hash = (n as u64).wrapping_mul(K) ^ head;
    hash = hash.wrapping_mul(K);
    hash ^= hash >> 32;
    hash = (hash ^ tail).wrapping_mul(K);
    hash ^ (hash >> 29)
}

#[cfg(test)]
mod tests;
