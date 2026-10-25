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
const SHAPE_SLOTS: usize = 128;
/// Longest key list a shape memo (and a table-mode segment) keeps.
const SHAPE_MAX_KEYS: usize = 256;
/// Containers a `flat_object` probe may nest below its starting object.
const FLAT_MAX_NEST: usize = 8;
/// Probes skipped after a failed one, per shape.
const FLAT_BACKOFF: u8 = 3;

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
    /// Probes of `flat_members` to skip after one failed from this shape, so a shape whose
    /// values never fit does not pay a failed scan per object.
    flat_skip: core::cell::Cell<u8>,
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
    /// Consecutive failed `flat_object` attempts, and the objects left to skip before the next probe.
    flat_misses: u8,
    flat_wait: u8,
}

impl FastScratch {
    /// Forgets every memoized shape. Call whenever the input slice may differ
    /// from the one previous `accept` calls saw.
    pub(crate) fn new_input(&mut self) {
        self.flat_misses = 0;
        self.flat_wait = 0;
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
    let _t = jmax_trace::span!(STRICT_FAST_RUN);
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
        memo_enabled: params.limits.reject_duplicate_keys
            && start <= input.len()
            && input.len() - start >= 256,
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
                    if let Some(end) = if self.memo_enabled {
                        self.flat_object(i)
                    } else {
                        None
                    } {
                        i = end;
                    } else {
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
                }
                b'[' => {
                    // Keep the matrix recognizer out of the hot path for the
                    // many flat arrays whose first item is a scalar. A small
                    // prefix probe is cheaper than inlining the full matrix
                    // scan (and its number parser) into this large loop.
                    let matrix_end = self
                        .numeric_matrix_candidate(i)
                        .then(|| self.numeric_matrix(i))
                        .flatten();
                    if let Some(end) = matrix_end.or_else(|| self.numeric_array(i)) {
                        i = end;
                    } else {
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
        } else {
            false
        };
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

    /// Fast path for flat numeric arrays. It has no per-item container state:
    /// the array's count is local, and an uncertain or nested value falls back
    /// before any scratch or decoded state has changed.
    #[inline(always)]
    fn numeric_array(&mut self, start: usize) -> Option<usize> {
        let _t = jmax_trace::span!(STRICT_FAST_ARRAY);
        if self.s.levels.len() >= self.p.max_depth {
            return None;
        }
        let input = self.input;
        let limits = self.p.limits;
        let numeric = self.p.numeric;
        let mut i = skip_ws(input, start + 1);
        if *input.get(i)? == b']' {
            return Some(i + 1);
        }
        if matches!(input.get(i), Some(b'-' | b'0'..=b'9')) {
            if let Some((end, negative_zero)) = integer_array(input, i, limits) {
                // Negative zero is the only integer spelling that can still be
                // rejected by a numeric policy. Let the shared number path decide.
                if numeric.is_none() || !negative_zero {
                    return Some(end);
                }
            }
        }
        let mut count = 0u32;
        loop {
            if count == limits.max_array_items {
                return None;
            }
            count += 1;
            // Keep the array scan on local state. Calling `self.number` for
            // every element reloads the same input, limits and policy fields.
            i = number_prefix(input, i, limits, numeric)?;
            i = skip_ws(input, i);
            match *input.get(i)? {
                b']' => return Some(i + 1),
                b',' => i = skip_ws(input, i + 1),
                _ => return None,
            }
        }
    }

    #[inline(always)]
    fn numeric_matrix_candidate(&self, start: usize) -> bool {
        let input = self.input;
        let row = skip_ws(input, start + 1);
        if input.get(row) != Some(&b'[') {
            return false;
        }
        let first = skip_ws(input, row + 1);
        matches!(input.get(first), Some(b'-' | b'0'..=b'9'))
    }

    /// Recognizes an array of two-number rows without pushing a frame per row.
    /// The scan is transactional: a miss leaves all parser state untouched.
    #[inline(never)]
    fn numeric_matrix(&self, start: usize) -> Option<usize> {
        let _t = jmax_trace::span!(STRICT_FAST_ARRAY);
        let input = self.input;
        let mut i = skip_ws(input, start + 1);
        if *input.get(i)? != b'['
            || self.s.levels.len().checked_add(2)? > self.p.max_depth
            || self.p.limits.max_array_items < 2
        {
            return None;
        }
        let limits = self.p.limits;
        let numeric = self.p.numeric;
        let mut rows = 0u32;
        loop {
            if rows == limits.max_array_items {
                return None;
            }
            if *input.get(i)? != b'[' {
                return None;
            }
            i = skip_ws(input, i + 1);
            i = coordinate_prefix(input, i, limits, numeric)?;
            i = skip_ws(input, i);
            if *input.get(i)? != b',' {
                return None;
            }
            i = skip_ws(input, i + 1);
            i = coordinate_prefix(input, i, limits, numeric)?;
            i = skip_ws(input, i);
            if *input.get(i)? != b']' {
                return None;
            }
            i = skip_ws(input, i + 1);
            rows += 1;
            match *input.get(i)? {
                b']' => return Some(i + 1),
                b',' => i = skip_ws(input, i + 1),
                _ => return None,
            }
        }
    }

    /// A key at `i` (its opening quote), the field count and duplicate checks,
    /// and the colon. Returns the offset after the colon.
    #[inline(always)]
    fn key(&mut self, i: usize) -> Option<usize> {
        let _t = jmax_trace::span!(STRICT_FAST_KEY);
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
        let _t = jmax_trace::span!(STRICT_FAST_SHAPE);
        let level = self.s.levels[top];
        let input = self.input;
        let content = i + 1;
        let hit = match self
            .s
            .shapes
            .get(level.shape as usize)?
            .keys
            .get(level.count as usize)
        {
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

    /// Recognizes a value tree that repeats memoized object shapes, without pushing a
    /// container frame per level: objects whose keys equal a recorded shape, arrays,
    /// and scalars. The memo recorded those keys unescaped, valid and pairwise distinct,
    /// so a byte-equal key needs no further check. The scan is transactional: it reads
    /// only, and applies its decoded-byte accounting on success, so a miss leaves all
    /// state untouched and the general loop decides, with every diagnostic.
    ///
    /// Gate: after a few consecutive misses the recognizer is only probed every 32nd
    /// object, so documents without repeated shapes pay almost nothing.
    #[inline(always)]
    fn flat_object(&mut self, start: usize) -> Option<usize> {
        if self.s.flat_misses >= 2 {
            if self.s.flat_wait > 0 {
                self.s.flat_wait -= 1;
                return None;
            }
            self.s.flat_wait = 32;
        }
        let found = self.flat_probe(start);
        self.s.flat_misses = if found.is_some() {
            0
        } else {
            self.s.flat_misses.saturating_add(1).min(2)
        };
        found
    }

    #[inline(never)]
    fn flat_probe(&mut self, start: usize) -> Option<usize> {
        let depth = self.s.levels.len();
        if self.s.shapes.is_empty() {
            return None;
        }
        let mut extra = 0usize;
        let end = self.flat_value(start, depth, depth, &mut extra)?;
        self.discarded = self.discarded.checked_add(u32::try_from(extra).ok()?)?;
        Some(end)
    }

    /// The value at `i` (not whitespace) whose container, if any, sits at `depth`.
    #[inline(always)]
    fn flat_value(&self, i: usize, depth: usize, base: usize, extra: &mut usize) -> Option<usize> {
        match *self.input.get(i)? {
            b'{' => self.flat_members(i, depth, base, extra),
            b'[' => self.flat_items(i, depth, base, extra),
            _ => self.flat_scalar(i, extra),
        }
    }

    #[inline(always)]
    fn flat_scalar(&self, i: usize, extra: &mut usize) -> Option<usize> {
        let input = self.input;
        match *input.get(i)? {
            b'"' => {
                let limits = self.p.limits;
                let content = i + 1;
                let (j, ascii) = find_special_ascii(input, content);
                match *input.get(j)? {
                    b'"' => {
                        if j - content > limits.max_string_bytes as usize {
                            return None;
                        }
                        if !ascii {
                            jmax_utf8::validate::validate_str(&input[content..j]).ok()?;
                        }
                        Some(j + 1)
                    }
                    b'\\' => {
                        let (end, len) = simple_escaped_string(input, i, j, ascii)?;
                        *extra = extra.checked_add(len)?;
                        let total = self
                            .decoded
                            .len()
                            .checked_add(self.discarded as usize)?
                            .checked_add(*extra)?;
                        if end - content > limits.max_string_bytes as usize
                            || total > limits.max_decoded_bytes as usize
                        {
                            return None;
                        }
                        Some(end + 1)
                    }
                    _ => None,
                }
            }
            b'-' | b'0'..=b'9' => number_prefix(input, i, self.p.limits, self.p.numeric),
            b't' => literal(input, i, b"true"),
            b'f' => literal(input, i, b"false"),
            b'n' => literal(input, i, b"null"),
            _ => None,
        }
    }

    /// The array at `start`, which would open at `depth`.
    fn flat_items(
        &self,
        start: usize,
        depth: usize,
        base: usize,
        extra: &mut usize,
    ) -> Option<usize> {
        if depth >= self.p.max_depth || depth - base >= FLAT_MAX_NEST {
            return None;
        }
        let input = self.input;
        let mut i = skip_ws(input, start + 1);
        if *input.get(i)? == b']' {
            return Some(i + 1);
        }
        let max_items = self.p.limits.max_array_items;
        let mut count = 0u32;
        loop {
            count += 1;
            if count > max_items {
                return None;
            }
            i = self.flat_value(i, depth + 1, base, extra)?;
            i = skip_ws(input, i);
            match *input.get(i)? {
                b',' => i = skip_ws(input, i + 1),
                b']' => return Some(i + 1),
                _ => return None,
            }
        }
    }

    /// The object at `start`, which would open at `depth`, spelled with the keys of a
    /// recorded shape, in order.
    fn flat_members(
        &self,
        start: usize,
        depth: usize,
        base: usize,
        extra: &mut usize,
    ) -> Option<usize> {
        if depth >= self.p.max_depth || depth - base >= FLAT_MAX_NEST {
            return None;
        }
        let input = self.input;
        let mut i = skip_ws(input, start + 1);
        match *input.get(i)? {
            b'}' => return Some(i + 1),
            b'"' => {}
            _ => return None,
        }
        let content = i + 1;
        let (close, _) = find_special_ascii(input, content);
        if *input.get(close)? != b'"' {
            return None;
        }
        let slot = shape_slot(depth, fingerprint(&input[content..close]));
        let shape = self.s.shapes.get(slot)?;
        if shape.generation != self.s.generation || shape.depth as usize != depth {
            return None;
        }
        if depth == base {
            let skip = shape.flat_skip.get();
            if skip > 0 {
                shape.flat_skip.set(skip - 1);
                return None;
            }
        }
        let found = self.flat_keys(&shape.keys, i, depth, base, extra);
        if found.is_none() && depth == base {
            shape.flat_skip.set(FLAT_BACKOFF);
        }
        found
    }

    /// The members of an object spelled with `keys`; `i` is the opening quote of its first key.
    fn flat_keys(
        &self,
        keys: &[ShapeKey],
        mut i: usize,
        depth: usize,
        base: usize,
        extra: &mut usize,
    ) -> Option<usize> {
        let input = self.input;
        let len = keys.len();
        if len > self.p.limits.max_fields_per_object as usize {
            return None;
        }
        let mut k = 0;
        loop {
            // The key's bytes and its closing quote; the opening quote is at `i`.
            let key = *keys.get(k)?;
            let n = key.len as usize + 1;
            let at = key.offset as usize;
            if !bytes_eq(input.get(i + 1..i + 1 + n)?, input.get(at..at + n)?) {
                return None;
            }
            i = skip_ws(input, i + 1 + n);
            if *input.get(i)? != b':' {
                return None;
            }
            i = skip_ws(input, i + 1);
            i = self.flat_value(i, depth + 1, base, extra)?;
            i = skip_ws(input, i);
            k += 1;
            match *input.get(i)? {
                b',' => {
                    i = skip_ws(input, i + 1);
                    if k == len || *input.get(i)? != b'"' {
                        return None;
                    }
                }
                b'}' => {
                    return if k == len { Some(i + 1) } else { None };
                }
                _ => return None,
            }
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
        let FastScratch {
            keys,
            shapes,
            generation,
            ..
        } = &mut *self.s;
        let segment = &keys[level.seg_start as usize..];
        let shape = &mut shapes[slot];
        shape.keys.clear();
        if shape.keys.try_reserve(segment.len()).is_err() {
            shape.generation = 0;
            return;
        }
        shape.generation = *generation;
        shape.depth = depth as u32;
        shape.flat_skip.set(0);
        shape.keys.extend(segment.iter().map(|e| ShapeKey {
            fingerprint: e.fingerprint,
            offset: e.offset,
            len: e.len,
        }));
    }

    fn check_unique(&mut self, cooked: Cooked) -> Option<()> {
        let _t = jmax_trace::span!(STRICT_FAST_SHAPE);
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
            let other = other_owner.get(
                entry.offset as usize..(entry.offset as usize).checked_add(entry.len as usize)?,
            )?;
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
            let existing = owner.get(
                entry.offset as usize..(entry.offset as usize).checked_add(entry.len as usize)?,
            )?;
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
                .insert_unique(
                    entry.fingerprint,
                    existing_ref,
                    existing,
                    input,
                    self.decoded,
                    0,
                )
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
        let _t = jmax_trace::span!(STRICT_FAST_STRING);
        let input = self.input;
        let content = i + 1;
        let (mut j, ascii) = find_special_ascii(input, content);
        let byte = *input.get(j)?;
        if byte == b'"' {
            let len = j - content;
            if len > self.p.limits.max_string_bytes as usize {
                return None;
            }
            let raw = &input[content..j];
            if !ascii {
                jmax_utf8::validate::validate_str(raw).ok()?;
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
        if !capture {
            if let Some((end, len)) = simple_escaped_string(input, i, j, ascii) {
                let total = self
                    .decoded
                    .len()
                    .checked_add(self.discarded as usize)?
                    .checked_add(len)?;
                if end - content > self.p.limits.max_string_bytes as usize
                    || total > self.p.limits.max_decoded_bytes as usize
                {
                    return None;
                }
                self.discarded = self.discarded.checked_add(u32::try_from(len).ok()?)?;
                return Some((
                    end + 1,
                    Cooked {
                        decoded: true,
                        offset: self.decoded.len() as u32,
                        len: len as u32,
                    },
                ));
            }
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
        let _t = jmax_trace::span!(STRICT_FAST_NUMBER);
        number_prefix(self.input, i, self.p.limits, self.p.numeric)
    }
}

/// Scans a flat array containing only JSON integers. A miss is transactional:
/// callers retry through `number_prefix`, which owns float and policy rules.
#[inline(never)]
fn integer_array(input: &[u8], mut i: usize, limits: Limits) -> Option<(usize, bool)> {
    let mut count = 0u32;
    let mut negative_zero = false;
    loop {
        if count == limits.max_array_items {
            return None;
        }
        count += 1;
        let start = i;
        if input.get(i) == Some(&b'-') {
            i += 1;
        }
        let first = *input.get(i)?;
        let digits_start = i;
        match first {
            b'0' => {
                negative_zero |= input[start] == b'-';
                i += 1;
            }
            b'1'..=b'9' => i = digit_run_end(input, i + 1),
            _ => return None,
        }
        if i - digits_start > usize::from(limits.max_number_digits) {
            return None;
        }
        i = skip_ws(input, i);
        match *input.get(i)? {
            b']' => return Some((i + 1, negative_zero)),
            b',' => i = skip_ws(input, i + 1),
            _ => return None,
        }
    }
}

/// Validate escaped strings (single-character and well-formed Unicode escapes) while finding their closing
/// quote. Raw non-ASCII runs are checked as UTF-8 (they sit between ASCII delimiters,
/// so each run is a whole-scalar sequence in any valid string) and pass through
/// unchanged, so the decoded length follows from the raw length and the escapes.
/// `prefix_ascii` says whether the bytes before the first backslash are ASCII.
/// A lone surrogate or any other doubt uses the established general decoder.
#[inline(never)]
fn simple_escaped_string(
    input: &[u8],
    opening: usize,
    mut slash: usize,
    prefix_ascii: bool,
) -> Option<(usize, usize)> {
    if !prefix_ascii {
        jmax_utf8::validate::validate_str(&input[opening + 1..slash]).ok()?;
    }
    // Raw bytes minus decoded bytes over all escapes seen so far.
    let mut shrink = 0usize;
    loop {
        let after = match *input.get(slash + 1)? {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                shrink += 1;
                slash + 2
            }
            b'u' => {
                let high = hex_quad(input, slash + 2)?;
                if (0xd800..=0xdbff).contains(&high) {
                    if input.get(slash + 6..slash + 8)? != b"\\u" {
                        return None;
                    }
                    let low = hex_quad(input, slash + 8)?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return None;
                    }
                    // Twelve raw bytes, one four-byte scalar.
                    shrink += 8;
                    slash + 12
                } else if (0xdc00..=0xdfff).contains(&high) {
                    return None;
                } else {
                    let decoded = if high < 0x80 {
                        1
                    } else if high < 0x800 {
                        2
                    } else {
                        3
                    };
                    shrink += 6 - decoded;
                    slash + 6
                }
            }
            _ => return None,
        };
        let (end, ascii) = find_special_ascii(input, after);
        if !ascii {
            jmax_utf8::validate::validate_str(input.get(after..end)?).ok()?;
        }
        match *input.get(end)? {
            b'"' => return Some((end, end - opening - 1 - shrink)),
            b'\\' => slash = end,
            _ => return None,
        }
    }
}

/// Four hex digits at `at`, as the exact decoder reads a Unicode escape.
#[inline(always)]
fn hex_quad(input: &[u8], at: usize) -> Option<u16> {
    let digits = input.get(at..at.checked_add(4)?)?;
    let mut value = 0u16;
    for &byte in digits {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | u16::from(digit);
    }
    Some(value)
}

/// Common short, nonzero-integer-part decimals. Other JSON numbers retain the
/// full scanner. At most sixteen fractional digits cannot change numeric-policy
/// acceptance when the magnitude is at least one and the integer part is short.
#[inline(always)]
fn coordinate_prefix(
    input: &[u8],
    start: usize,
    limits: Limits,
    numeric: Option<NumericPolicy>,
) -> Option<usize> {
    if let Some(window) = input.get(start..start.checked_add(32)?) {
        let sign = usize::from(window[0] == b'-');
        if matches!(window[sign], b'1'..=b'9') {
            let integer = (non_digit_mask(load_word(window, sign)).trailing_zeros() / 8) as usize;
            let dot = sign + integer;
            if integer < 8 && window[dot] == b'.' {
                let fraction = dot + 1;
                let first = non_digit_mask(load_word(window, fraction));
                let fractional = if first == 0 {
                    8 + (non_digit_mask(load_word(window, fraction + 8)).trailing_zeros() / 8)
                        as usize
                } else {
                    (first.trailing_zeros() / 8) as usize
                };
                let end = fraction + fractional;
                if fractional != 0
                    && fractional < 16
                    && integer + fractional <= usize::from(limits.max_number_digits)
                    && jmax_number::is_number_delimiter(window[end])
                {
                    return Some(start + end);
                }
            }
        }
    }
    number_prefix(input, start, limits, numeric)
}

#[inline(always)]
pub(crate) fn number_prefix(
    input: &[u8],
    i: usize,
    limits: Limits,
    numeric: Option<NumericPolicy>,
) -> Option<usize> {
    let n = input.len();
    let mut j = i;
    let negative = *input.get(j)? == b'-';
    if negative {
        j += 1;
    }
    let first = *input.get(j)?;
    let int_start = j;
    match first {
        b'0' => j += 1,
        b'1'..=b'9' => {
            j += 1;
            j = digit_run_end(input, j);
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
        j = digit_run_end(input, j);
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
        j = digit_run_end(input, j);
        if j == start || j - start > 12 {
            return None;
        }
        digits += j - start;
    }
    if j < n && !jmax_number::is_number_delimiter(input[j]) {
        return None;
    }
    if digits > usize::from(limits.max_number_digits) {
        return None;
    }
    let negative_zero = negative && first == b'0';
    if numeric.is_some() && (float || negative_zero) {
        let exact = exponent || negative_zero || j - i > POLICY_SAFE_FLOAT_BYTES;
        if exact && !super::feed::number_accepted_fast(&input[i..j], numeric) {
            return None;
        }
    }
    Some(j)
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
        let rd =
            |s: &[u8], at: usize| u64::from_le_bytes(s[at..at + 8].try_into().unwrap_or_default());
        return rd(a, 0) == rd(b, 0) && rd(a, n - 8) == rd(b, n - 8);
    }
    if n >= 4 && n < 8 {
        let rd =
            |s: &[u8], at: usize| u32::from_le_bytes(s[at..at + 4].try_into().unwrap_or_default());
        return rd(a, 0) == rd(b, 0) && rd(a, n - 4) == rd(b, n - 4);
    }
    a == b
}

/// Direct-mapped slot for the shape of an object at `depth` starting with a key of this fingerprint.
#[inline(always)]
fn shape_slot(depth: usize, fingerprint: u64) -> usize {
    let mixed = fingerprint ^ (depth as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed >> 57) as usize
}

const LOW_ONES: u64 = 0x0101_0101_0101_0101;
const HIGH_BITS: u64 = 0x8080_8080_8080_8080;

/// Marks the first non-digit exactly; later flags may include carry artifacts.
#[inline(always)]
fn non_digit_mask(word: u64) -> u64 {
    (word.wrapping_sub(LOW_ONES * 0x30) | word.wrapping_add(LOW_ONES * 0x46)) & HIGH_BITS
}

/// Scans a decimal digit run in bounded words, then handles its short tail.
#[inline(always)]
fn digit_run_end(input: &[u8], mut i: usize) -> usize {
    while input.len() - i >= 8 {
        let mask = non_digit_mask(load_word(input, i));
        if mask != 0 {
            return i + (mask.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while let Some(&byte) = input.get(i) {
        if !byte.is_ascii_digit() {
            break;
        }
        i += 1;
    }
    i
}

#[inline(always)]
fn load_word(input: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(input[i..i + 8].try_into().unwrap_or_default())
}

/// Whitespace, with whole words of spaces (indentation) skipped at a time.
#[inline(always)]
fn skip_ws(input: &[u8], i: usize) -> usize {
    if !matches!(input.get(i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        return i;
    }
    let next = i + 1;
    if !matches!(input.get(next), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        return next;
    }
    let rest = &input[next..];
    // Indentation runs are almost always shorter than one vector: settle them inline.
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    if rest.len() >= 16 {
        let run = unsafe { whitespace_head_sse2(rest) };
        if run < 16 {
            return next + run;
        }
        return next + 16 + whitespace_prefix(&rest[16..]);
    }
    next + whitespace_prefix(rest)
}

/// Whitespace prefix length within the first 16 bytes (16 when all of them are whitespace).
/// The caller guarantees at least 16 bytes.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[inline(always)]
unsafe fn whitespace_head_sse2(input: &[u8]) -> usize {
    use core::arch::x86_64 as arch;
    unsafe {
        let bytes = arch::_mm_loadu_si128(input.as_ptr().cast());
        let ws = arch::_mm_or_si128(
            arch::_mm_or_si128(
                arch::_mm_cmpeq_epi8(bytes, arch::_mm_set1_epi8(b' ' as i8)),
                arch::_mm_cmpeq_epi8(bytes, arch::_mm_set1_epi8(b'\n' as i8)),
            ),
            arch::_mm_or_si128(
                arch::_mm_cmpeq_epi8(bytes, arch::_mm_set1_epi8(b'\r' as i8)),
                arch::_mm_cmpeq_epi8(bytes, arch::_mm_set1_epi8(b'\t' as i8)),
            ),
        );
        (!(arch::_mm_movemask_epi8(ws) as u32))
            .trailing_zeros()
            .min(16) as usize
    }
}

/// Bounded local whitespace scan. Short runs avoid vector setup; tails use
/// eight-byte SWAR words and then exact scalar classification.
#[inline(never)]
fn whitespace_prefix(input: &[u8]) -> usize {
    if input.len() >= 16 {
        #[cfg(target_arch = "aarch64")]
        {
            return unsafe { whitespace_neon(input) };
        }
        #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
        {
            return unsafe { whitespace_sse2(input) };
        }
    }
    whitespace_swar(input)
}

#[inline(always)]
fn whitespace_swar(input: &[u8]) -> usize {
    let mut at = 0;
    while input.len() - at >= 8 {
        let word = load_word(input, at);
        let mask = zero_byte_mask_exact(word ^ (SWAR_LOW * u64::from(b' ')))
            | zero_byte_mask_exact(word ^ (SWAR_LOW * u64::from(b'\n')))
            | zero_byte_mask_exact(word ^ (SWAR_LOW * u64::from(b'\r')))
            | zero_byte_mask_exact(word ^ (SWAR_LOW * u64::from(b'\t')));
        if mask != SWAR_HIGH {
            let non_ws = !mask & SWAR_HIGH;
            return at + (non_ws.trailing_zeros() / 8) as usize;
        }
        at += 8;
    }
    at + input[at..]
        .iter()
        .take_while(|&&b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        .count()
}

/// Index of the first byte at or after `i` that is `"`, `\` or below 0x20, or
/// the input length. Dispatches to the architecture-selected bounded scanner.
#[inline(always)]
fn find_special(input: &[u8], i: usize) -> usize {
    let bytes = &input[i..];
    if bytes.len() >= 16 {
        #[cfg(target_arch = "aarch64")]
        {
            return i + unsafe { special_neon(bytes) };
        }
        #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
        {
            return i + unsafe { special_sse2(bytes) };
        }
    }
    i + special_ascii_swar(bytes).0
}

/// Finds the first boundary and ASCII status with a local SWAR/SIMD scan.
#[inline(always)]
fn find_special_ascii(input: &[u8], i: usize) -> (usize, bool) {
    let bytes = &input[i..];
    #[cfg(target_arch = "aarch64")]
    if bytes.len() >= 16 {
        let (end, ascii) = unsafe { special_ascii_neon(bytes) };
        return (i + end, ascii);
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    if bytes.len() >= 16 {
        let (end, ascii) = unsafe { special_ascii_sse2(bytes) };
        return (i + end, ascii);
    }
    let (end, ascii) = special_ascii_swar(bytes);
    (i + end, ascii)
}

#[inline(always)]
fn scalar_special_ascii(input: &[u8]) -> (usize, bool) {
    let mut ascii = true;
    for (at, &byte) in input.iter().enumerate() {
        if byte == b'"' || byte == b'\\' || byte < 0x20 {
            return (at, ascii);
        }
        ascii &= byte < 0x80;
    }
    (input.len(), ascii)
}

const SWAR_LOW7: u64 = 0x7f7f_7f7f_7f7f_7f7f;
const SWAR_HIGH: u64 = 0x8080_8080_8080_8080;
const SWAR_LOW: u64 = 0x0101_0101_0101_0101;

#[inline(always)]
fn zero_byte_mask(word: u64) -> u64 {
    word.wrapping_sub(SWAR_LOW) & !word & SWAR_HIGH
}
#[inline(always)]
fn zero_byte_mask_exact(word: u64) -> u64 {
    !((word & SWAR_LOW7).wrapping_add(SWAR_LOW7) | word) & SWAR_HIGH
}

/// Finds special bytes with one bounded 8-byte read per word.
#[inline(always)]
fn special_ascii_swar(input: &[u8]) -> (usize, bool) {
    let mut at = 0;
    let mut ascii = true;
    while input.len() - at >= 8 {
        let word = load_word(input, at);
        let quote = zero_byte_mask(word ^ (SWAR_LOW * u64::from(b'"')));
        let slash = zero_byte_mask(word ^ (SWAR_LOW * u64::from(b'\\')));
        let ge_space = ((word & SWAR_LOW7).wrapping_add(SWAR_LOW * 0x60)) & SWAR_HIGH;
        let controls = !ge_space & !word & SWAR_HIGH;
        let special = quote | slash | controls;
        let high = word & SWAR_HIGH;
        if special != 0 {
            let lane = (special.trailing_zeros() / 8) as usize;
            let before = if lane == 0 {
                0
            } else {
                (1u64 << (lane * 8)) - 1
            };
            return (at + lane, ascii && (high & before) == 0);
        }
        ascii &= high == 0;
        at += 8;
    }
    let tail = scalar_special_ascii(&input[at..]);
    (at + tail.0, ascii && tail.1)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline(always)]
unsafe fn special_ascii_sse2(input: &[u8]) -> (usize, bool) {
    #[cfg(target_arch = "x86")]
    use core::arch::x86 as arch;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64 as arch;
    unsafe {
        let quote = arch::_mm_set1_epi8(b'"' as i8);
        let slash = arch::_mm_set1_epi8(b'\\' as i8);
        let limit = arch::_mm_set1_epi8(0x1f);
        let mut offset = 0;
        let mut ascii = true;
        while input.len() - offset >= 16 {
            let bytes = arch::_mm_loadu_si128(input.as_ptr().add(offset).cast());
            let special = arch::_mm_or_si128(
                arch::_mm_or_si128(
                    arch::_mm_cmpeq_epi8(bytes, quote),
                    arch::_mm_cmpeq_epi8(bytes, slash),
                ),
                arch::_mm_cmpeq_epi8(arch::_mm_min_epu8(bytes, limit), bytes),
            );
            let mask = arch::_mm_movemask_epi8(special) as u32;
            let high = arch::_mm_movemask_epi8(bytes) as u32;
            if mask != 0 {
                let lane = mask.trailing_zeros() as usize;
                let prefix = (1u32 << lane).wrapping_sub(1);
                return (offset + lane, ascii && high & prefix == 0);
            }
            ascii &= high == 0;
            offset += 16;
        }
        let tail = special_ascii_swar(&input[offset..]);
        (offset + tail.0, ascii && tail.1)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn special_sse2(input: &[u8]) -> usize {
    use core::arch::x86_64 as arch;
    unsafe {
        let quote = arch::_mm_set1_epi8(b'"' as i8);
        let slash = arch::_mm_set1_epi8(b'\\' as i8);
        let limit = arch::_mm_set1_epi8(0x1f);
        let mut offset = 0;
        while input.len() - offset >= 16 {
            let bytes = arch::_mm_loadu_si128(input.as_ptr().add(offset).cast());
            let special = arch::_mm_or_si128(
                arch::_mm_or_si128(
                    arch::_mm_cmpeq_epi8(bytes, quote),
                    arch::_mm_cmpeq_epi8(bytes, slash),
                ),
                arch::_mm_cmpeq_epi8(arch::_mm_min_epu8(bytes, limit), bytes),
            );
            let mask = arch::_mm_movemask_epi8(special) as u32;
            if mask != 0 {
                return offset + mask.trailing_zeros() as usize;
            }
            offset += 16;
        }
        offset + special_ascii_swar(&input[offset..]).0
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn special_ascii_neon(input: &[u8]) -> (usize, bool) {
    use core::arch::aarch64::*;
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let below_space = vdupq_n_u8(b' ');
        let mut chunks = input.chunks_exact(16);
        let mut offset = 0;
        let mut ascii = true;
        for chunk in &mut chunks {
            let bytes = vld1q_u8(chunk.as_ptr());
            let special = vorrq_u8(
                vorrq_u8(vceqq_u8(bytes, quote), vceqq_u8(bytes, slash)),
                vcltq_u8(bytes, below_space),
            );
            let packed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(special));
            let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(packed));
            let first = (mask.trailing_zeros() / 4) as usize;
            let all_ascii = vmaxvq_u8(bytes) < 0x80;
            if first != 16 {
                if !all_ascii {
                    let high = vcgtq_u8(bytes, vdupq_n_u8(0x7f));
                    let high = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                        vreinterpretq_u16_u8(high),
                    )));
                    let prefix = (1u64 << (first * 4)) - 1;
                    ascii &= high & prefix == 0;
                }
                return (offset + first, ascii);
            }
            ascii &= all_ascii;
            offset += 16;
        }
        let (tail, tail_ascii) = scalar_special_ascii(chunks.remainder());
        (offset + tail, ascii && tail_ascii)
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

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn special_neon(input: &[u8]) -> usize {
    use core::arch::aarch64::*;
    unsafe {
        let quote = vdupq_n_u8(b'"');
        let slash = vdupq_n_u8(b'\\');
        let space = vdupq_n_u8(b' ');
        let mut at = 0;
        while input.len() - at >= 32 {
            let b0 = vld1q_u8(input.as_ptr().add(at));
            let b1 = vld1q_u8(input.as_ptr().add(at + 16));
            let s0 = neon_special_mask(b0, quote, slash, space);
            let s1 = neon_special_mask(b1, quote, slash, space);
            if s0 | s1 != 0 {
                return at
                    + if s0 != 0 {
                        (s0.trailing_zeros() / 4) as usize
                    } else {
                        16 + (s1.trailing_zeros() / 4) as usize
                    };
            }
            at += 32;
        }
        if input.len() - at >= 16 {
            let mask = neon_special_mask(vld1q_u8(input.as_ptr().add(at)), quote, slash, space);
            if mask != 0 {
                return at + (mask.trailing_zeros() / 4) as usize;
            }
            at += 16;
        }
        at + scalar_special_ascii(&input[at..]).0
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn whitespace_neon(input: &[u8]) -> usize {
    use core::arch::aarch64::*;
    unsafe {
        let spaces = vdupq_n_u8(b' ');
        let lf = vdupq_n_u8(b'\n');
        let cr = vdupq_n_u8(b'\r');
        let tab = vdupq_n_u8(b'\t');
        let mut offset = 0;
        while input.len() - offset >= 16 {
            let bytes = vld1q_u8(input.as_ptr().add(offset));
            let ws = vorrq_u8(
                vorrq_u8(vceqq_u8(bytes, spaces), vceqq_u8(bytes, lf)),
                vorrq_u8(vceqq_u8(bytes, cr), vceqq_u8(bytes, tab)),
            );
            let packed = vshrn_n_u16::<4>(vreinterpretq_u16_u8(ws));
            let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(packed));
            let first = ((!mask).trailing_zeros() / 4) as usize;
            if first != 16 {
                return offset + first;
            }
            offset += 16;
        }
        offset + whitespace_swar(&input[offset..])
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn whitespace_sse2(input: &[u8]) -> usize {
    use core::arch::x86_64 as arch;
    unsafe {
        let spaces = arch::_mm_set1_epi8(b' ' as i8);
        let lf = arch::_mm_set1_epi8(b'\n' as i8);
        let cr = arch::_mm_set1_epi8(b'\r' as i8);
        let tab = arch::_mm_set1_epi8(b'\t' as i8);
        let mut offset = 0;
        while input.len() - offset >= 16 {
            let bytes = arch::_mm_loadu_si128(input.as_ptr().add(offset).cast());
            let ws = arch::_mm_or_si128(
                arch::_mm_or_si128(
                    arch::_mm_cmpeq_epi8(bytes, spaces),
                    arch::_mm_cmpeq_epi8(bytes, lf),
                ),
                arch::_mm_or_si128(
                    arch::_mm_cmpeq_epi8(bytes, cr),
                    arch::_mm_cmpeq_epi8(bytes, tab),
                ),
            );
            let mask = arch::_mm_movemask_epi8(ws) as u32;
            if mask != u16::MAX as u32 {
                return offset + (!mask).trailing_zeros() as usize;
            }
            offset += 16;
        }
        offset + whitespace_swar(&input[offset..])
    }
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
        let word =
            u64::from(bytes[0]) | u64::from(bytes[n / 2]) << 8 | u64::from(bytes[n - 1]) << 16;
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
