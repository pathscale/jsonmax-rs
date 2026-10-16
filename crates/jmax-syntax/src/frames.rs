//! Container stack updates, inclusive resource limits, and decoded key identity.
//!
//! All fallible work precedes structural mutation. A failed reservation may
//! change storage capacity, but never the set of keys, counters, or phases.

use super::*;

/// One reusable table per nesting depth. Entries borrow raw or cooked bytes by
/// offset, so arena growth and input-vector reallocation cannot invalidate keys.
/// Generations make reuse by sibling objects constant time without clearing a
/// table sized for a previous large object.
#[derive(Debug)]
pub(crate) struct KeyTable {
    slots: Vec<KeySlot>,
    len: usize,
    generation: u32,
}

#[derive(Clone, Copy, Debug)]
struct KeySlot {
    hash: u64,
    reference: StrRef,
    generation: u32,
}

impl KeySlot {
    const EMPTY: Self = Self {
        hash: 0,
        reference: StrRef {
            span: Span32::new(0, 0),
            source: SpanSource::RawPayload,
            scalar_len: 0,
        },
        generation: 0,
    };
}

impl KeyTable {
    pub(crate) fn new() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            generation: 1,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.len = 0;
        match self.generation.checked_add(1) {
            Some(generation) => self.generation = generation,
            None => {
                self.slots.fill(KeySlot::EMPTY);
                self.generation = 1;
            }
        }
    }

    pub(crate) fn contains(
        &self,
        hash: u64,
        bytes: &[u8],
        raw: &[u8],
        decoded: &[u8],
        offset: u32,
    ) -> Result<bool> {
        if self.slots.is_empty() {
            return Ok(false);
        }
        let mask = self.slots.len() - 1;
        let mut index = hash as usize & mask;
        loop {
            let slot = self.slots[index];
            if slot.generation != self.generation {
                return Ok(false);
            }
            if slot.hash == hash {
                let owner = match slot.reference.source {
                    SpanSource::RawPayload => raw,
                    SpanSource::DecodedArena => decoded,
                };
                let seen = slot.reference.span.slice(owner).ok_or_else(|| {
                    syntax_error(
                        ErrorCode::InternalError,
                        offset,
                        "duplicate key reference is out of bounds",
                    )
                })?;
                if seen == bytes {
                    return Ok(true);
                }
            }
            index = (index + 1) & mask;
        }
    }

    /// Keep load at or below one half. Allocation and rehashing precede the
    /// parser's phase/count commit, and preserve every existing key on failure.
    pub(crate) fn reserve_insert(&mut self, offset: u32) -> Result<()> {
        if self.len < self.slots.len() / 2 {
            return Ok(());
        }
        let capacity = if self.slots.is_empty() {
            8
        } else {
            self.slots.len().checked_mul(2).ok_or_else(|| {
                syntax_error(
                    ErrorCode::BudgetExhausted,
                    offset,
                    "duplicate key table capacity overflow",
                )
            })?
        };
        let mut slots = Vec::new();
        reserve(&mut slots, capacity, offset)?;
        slots.resize(capacity, KeySlot::EMPTY);
        let mask = capacity - 1;
        for slot in &self.slots {
            if slot.generation == self.generation {
                let mut index = slot.hash as usize & mask;
                while slots[index].generation == self.generation {
                    index = (index + 1) & mask;
                }
                slots[index] = *slot;
            }
        }
        self.slots = slots;
        Ok(())
    }

    /// Called only after reserve_insert, with at least half the slots empty.
    pub(crate) fn insert_reserved(&mut self, hash: u64, reference: StrRef) {
        let mask = self.slots.len() - 1;
        let mut index = hash as usize & mask;
        while self.slots[index].generation == self.generation {
            index = (index + 1) & mask;
        }
        self.slots[index] = KeySlot {
            hash,
            reference,
            generation: self.generation,
        };
        self.len += 1;
    }
}

/// Returns the innermost container phase, or the root phase without a frame.
pub fn current_phase(state: &SyntaxState) -> FramePhase {
    state
        .frames
        .last()
        .map_or(state.root_phase, |frame| frame.phase)
}

/// Checks retained payload growth against both the ABI and configured ceiling.
///
/// The error offset is the current length, saturated to the ABI's offset range.
/// This function performs no allocation and does not mutate parser state.
pub fn check_payload_growth(current: usize, additional: usize, limits: Limits) -> Result<u32> {
    let offset = u32::try_from(current).unwrap_or(u32::MAX);
    current
        .checked_add(additional)
        .and_then(|total| u32::try_from(total).ok())
        .filter(|&total| total <= limits.max_payload_bytes)
        .ok_or_else(|| {
            syntax_error(
                ErrorCode::PayloadTooLarge,
                offset,
                "payload exceeds its byte limit or offset range",
            )
        })
}

/// Checks the grammar exactly once and commits its validated transition.
pub(crate) fn apply_transition(
    state: &mut SyntaxState,
    token: Token,
    cooked_hash: Option<u64>,
) -> Result<Transition> {
    if let Some(error) = state.failure.as_ref() {
        return Err(error.clone());
    }
    let result = (|| {
        let transition = machine::transition(current_phase(state), token)?;
        apply_parts(
            state.limits,
            state.duplicate_owner,
            &state.input,
            &state.decoded,
            &mut state.frames,
            &mut state.key_tables,
            &mut state.root_phase,
            token,
            transition,
            cooked_hash,
        )?;
        Ok(transition)
    })();
    result.map_err(|error| state.fail(error))
}

/// Internal commit: `transition` must have just been checked for these frames.
/// Kept private to syntax so callers cannot commit arbitrary public transitions.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_parts(
    limits: Limits,
    duplicate_owner: DuplicatePolicyOwner,
    raw: &[u8],
    decoded: &[u8],
    frames: &mut Vec<Frame>,
    key_tables: &mut Vec<KeyTable>,
    root_phase: &mut FramePhase,
    token: Token,
    transition: Transition,
    cooked_hash: Option<u64>,
) -> Result<()> {
    let offset = token.span.offset;
    let inconsistent = || {
        syntax_error(
            ErrorCode::InternalError,
            offset,
            "inconsistent syntax frame or transition",
        )
    };
    if transition.action == FrameAction::Pop {
        let frame = frames.last().ok_or_else(inconsistent)?;
        if !matches!(
            (frame.kind, token.kind),
            (ContainerKind::Object, TokenKind::RBrace)
                | (ContainerKind::Array, TokenKind::RBracket)
        ) {
            return Err(inconsistent());
        }
        // The departing next_phase must not overwrite the saved parent phase.
        return frames.pop().map(|_| ()).ok_or_else(inconsistent);
    }

    let child = match transition.action {
        FrameAction::PushObject => Some((ContainerKind::Object, FramePhase::ObjectStart)),
        FrameAction::PushArray => Some((ContainerKind::Array, FramePhase::ArrayStart)),
        FrameAction::None => None,
        FrameAction::Pop => return Err(inconsistent()),
    };
    if child.is_some() && frames.len() >= usize::from(limits.max_depth) {
        return Err(syntax_error(
            ErrorCode::DepthLimitExceeded,
            offset,
            "container depth exceeds its limit",
        ));
    }

    let key = match transition.event {
        Some(Event::Key { str_ref }) => Some(str_ref),
        _ => None,
    };
    let next_count = match frames.last() {
        Some(frame) if key.is_some() => {
            if frame.kind != ContainerKind::Object {
                return Err(inconsistent());
            }
            Some(increment_count(
                frame.count,
                limits.max_fields_per_object,
                offset,
                "object field count exceeds its limit",
            )?)
        }
        Some(frame)
            if frame.kind == ContainerKind::Array && machine::is_value_start(token.kind) =>
        {
            Some(increment_count(
                frame.count,
                limits.max_array_items,
                offset,
                "array item count exceeds its limit",
            )?)
        }
        None if key.is_some() => return Err(inconsistent()),
        _ => None,
    };

    let depth = frames.len();
    let mut key_entry = None;
    if limits.reject_duplicate_keys && duplicate_owner == DuplicatePolicyOwner::Syntax {
        if let Some(reference) = key {
            let owner = match reference.source {
                SpanSource::RawPayload => raw,
                SpanSource::DecodedArena => decoded,
            };
            let bytes = reference.span.slice(owner).ok_or_else(inconsistent)?;
            let hash = cooked_hash.unwrap_or_else(|| KeyMeta::compute_fnv1a64(bytes));
            let table = depth
                .checked_sub(1)
                .and_then(|index| key_tables.get(index))
                .ok_or_else(inconsistent)?;
            if table.contains(hash, bytes, raw, decoded, offset)? {
                return Err(syntax_error(
                    ErrorCode::DuplicateObjectKey,
                    offset,
                    "duplicate decoded object key",
                ));
            }
            key_tables[depth - 1].reserve_insert(offset)?;
            key_entry = Some((hash, reference));
        }
    }
    if child.is_some() {
        reserve(frames, 1, offset)?;
        if key_tables.len() == depth {
            reserve(key_tables, 1, offset)?;
        }
    }

    // All checks and reservations have succeeded. The following pushes cannot
    // allocate. Unknown-key policy belongs to schema validation, not framing.
    match frames.last_mut() {
        Some(frame) => {
            frame.phase = transition.next_phase;
            if let Some(count) = next_count {
                frame.count = count;
            }
        }
        None => {
            if next_count.is_some() || key_entry.is_some() {
                return Err(inconsistent());
            }
            *root_phase = transition.next_phase;
        }
    }
    if let Some((hash, reference)) = key_entry {
        key_tables[depth - 1].insert_reserved(hash, reference);
    }
    if let Some((kind, phase)) = child {
        if key_tables.len() == depth {
            key_tables.push(KeyTable::new());
        }
        key_tables[depth].reset();
        frames.push(Frame {
            kind,
            phase,
            start_offset: offset,
            count: 0,
            decoded_checkpoint: decoded.len(),
        });
    }
    Ok(())
}

pub(crate) fn increment_count(
    count: u32,
    limit: u32,
    offset: u32,
    detail: &'static str,
) -> Result<u32> {
    count
        .checked_add(1)
        .filter(|&next| next <= limit)
        .ok_or_else(|| syntax_error(ErrorCode::ConstraintViolation, offset, detail))
}

fn reserve<T>(storage: &mut Vec<T>, additional: usize, offset: u32) -> Result<()> {
    storage.try_reserve(additional).map_err(|_| {
        syntax_error(
            ErrorCode::BudgetExhausted,
            offset,
            "unable to reserve syntax frame storage",
        )
    })
}
