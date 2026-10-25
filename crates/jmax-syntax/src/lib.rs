//! Incremental JSON syntax state. References use absolute, owner-relative offsets.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec::Vec;
pub mod block_cursor;
pub use block_cursor::{BlockCursor, SkipScratch, skip_strict, skip_strict_continuing};
pub mod feed;
pub mod finish;
pub mod frames;
pub mod machine;
mod strict_fast;

/// Index of the first byte at or after `i` that is not JSON whitespace (space, tab, CR, LF),
/// or `input.len()` when only whitespace remains. A short run is settled inline and a long one
/// by a vector scan, so indentation does not cost a branch per byte.
#[inline(always)]
pub fn skip_json_whitespace(input: &[u8], i: usize) -> usize {
    strict_fast::skip_ws(input, i)
}

pub use jmax_abi::buffer::Payload;
pub use jmax_abi::buffer::{ValidationCapability, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::{
    CursorState, DuplicatePolicyOwner, Event, FramePhase, KeyMeta, Limits, ScalarKind, ScanStep,
    Scratch, StringMeta, TokenKind,
};

pub const JSON_WHITESPACE: [u8; 4] = [b' ', b'\t', b'\r', b'\n'];

/// A complete lexical token; strings carry a validated, cooked reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAction {
    None,
    PushObject,
    PushArray,
    Pop,
}

/// Parent phase advances when a value starts, before a child frame is pushed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub next_phase: FramePhase,
    pub action: FrameAction,
    pub event: Option<Event>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerKind {
    Object,
    Array,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingKind {
    String,
    Number,
    Literal(TokenKind),
}

/// Boundary discovery carry. Decoding occurs once the complete token is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingToken {
    pub kind: PendingKind,
    pub start: u32,
    pub scan_offset: u32,
    pub first_escape: Option<u32>,
    pub non_ascii: bool,
    pub control: bool,
    pub digits: u32,
    pub excess_digit: Option<u32>,
}

/// Complete input retains its original shared owner. Only incremental input
/// needs a growable byte buffer; neither traversal nor finalization copies it.
#[derive(Debug)]
pub(crate) enum InputStorage {
    Incremental(Vec<u8>),
    Complete(Payload),
}

impl InputStorage {
    pub(crate) fn into_payload(self) -> Payload {
        match self {
            Self::Incremental(bytes) => Payload::from_vec(bytes),
            Self::Complete(payload) => payload,
        }
    }
}

impl core::ops::Deref for InputStorage {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Incremental(bytes) => bytes,
            Self::Complete(payload) => payload.as_slice(),
        }
    }
}

/// One cached block, including the carry needed to extend a partial tail after
/// another feed. Classification may look ahead, but never consumes an event.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClassifiedBlock {
    pub start: u32,
    pub end: u32,
    pub before: jmax_dispatch::ScanCarry,
    pub masks: jmax_dispatch::BlockMasks,
}

/// Root is represented separately and does not consume container depth.
#[derive(Clone, Debug)]
pub struct Frame {
    pub(crate) kind: ContainerKind,
    pub(crate) phase: FramePhase,
    pub(crate) start_offset: u32,
    pub(crate) count: u32,
    // Keys decoded within this container can be reclaimed when strict discard
    // closes it; ancestors' keys precede this checkpoint and remain live.
    pub(crate) decoded_checkpoint: usize,
}

impl Frame {
    pub const fn kind(&self) -> ContainerKind {
        self.kind
    }
    pub const fn phase(&self) -> FramePhase {
        self.phase
    }
    pub const fn start_offset(&self) -> u32 {
        self.start_offset
    }
    pub const fn count(&self) -> u32 {
        self.count
    }
}

/// Retains input ownership so previously emitted references survive further feeds.
/// Once an error is recorded, every mutating operation returns that error.
#[derive(Debug)]
pub struct SyntaxState {
    pub(crate) limits: Limits,
    pub(crate) numeric: Option<jmax_abi::number::NumericPolicy>,
    pub(crate) duplicate_owner: DuplicatePolicyOwner,
    pub(crate) complete_cache: Option<block_cursor::CursorCache>,
    pub(crate) discard_scratch: block_cursor::DiscardScratch,
    pub(crate) input: InputStorage,
    pub(crate) decoded: Vec<u8>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) root_phase: FramePhase,
    pub(crate) byte_offset: u32,
    pub(crate) final_input: bool,
    pub(crate) finished: bool,
    pub(crate) query_skipped: bool,
    pub(crate) failure: Option<Error>,
    pub(crate) pending: Option<PendingToken>,
    pub(crate) key_tables: Vec<frames::KeyTable>,
    pub(crate) dispatch: Option<jmax_dispatch::Dispatch>,
    pub(crate) block: Option<ClassifiedBlock>,
    pub(crate) scan_offset: u32,
    pub(crate) scan_carry: jmax_dispatch::ScanCarry,
}

impl SyntaxState {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            numeric: None,
            duplicate_owner: DuplicatePolicyOwner::Syntax,
            complete_cache: None,
            discard_scratch: block_cursor::DiscardScratch::default(),
            input: InputStorage::Incremental(Vec::new()),
            decoded: Vec::new(),
            frames: Vec::new(),
            root_phase: FramePhase::RootStart,
            byte_offset: 0,
            final_input: false,
            finished: false,
            query_skipped: false,
            failure: None,
            pending: None,
            key_tables: Vec::new(),
            dispatch: None,
            block: None,
            scan_offset: 0,
            scan_carry: jmax_dispatch::ScanCarry::default(),
        }
    }

    /// Retains a complete payload without copying its bytes and declares EOF.
    /// Syntax errors are diagnosed lazily as events are consumed.
    pub fn from_payload(payload: Payload, limits: Limits) -> Result<Self> {
        frames::check_payload_growth(0, payload.len(), limits)?;
        let mut state = Self::new(limits);
        state.input = InputStorage::Complete(payload);
        state.final_input = true;
        Ok(state)
    }

    /// Retains complete input and enforces conversion policy as numbers arrive.
    pub fn from_payload_with_numeric(
        payload: Payload,
        limits: Limits,
        numeric: jmax_abi::number::NumericPolicy,
    ) -> Result<Self> {
        if numeric.max_exponent_abs < 0 {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                0,
                "numeric exponent limit cannot be negative",
            ));
        }
        let mut state = Self::from_payload(payload, limits)?;
        state.numeric = Some(numeric);
        Ok(state)
    }
    /// Start another complete input, retaining frames, duplicate tables and arena.
    pub fn reset_with_payload(
        &mut self,
        payload: Payload,
        limits: Limits,
        numeric: Option<jmax_abi::number::NumericPolicy>,
    ) -> Result<()> {
        frames::check_payload_growth(0, payload.len(), limits)?;
        if numeric.is_some_and(|p| p.max_exponent_abs < 0) {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                0,
                "numeric exponent limit cannot be negative",
            ));
        }
        self.complete_cache = None;
        self.duplicate_owner = DuplicatePolicyOwner::Syntax;
        self.limits = limits;
        self.numeric = numeric;
        self.input = InputStorage::Complete(payload);
        self.decoded.clear();
        self.frames.clear();
        self.root_phase = FramePhase::RootStart;
        self.byte_offset = 0;
        self.final_input = true;
        self.finished = false;
        self.query_skipped = false;
        self.failure = None;
        self.pending = None;
        self.dispatch = None;
        self.block = None;
        self.scan_offset = 0;
        self.scan_carry = jmax_dispatch::ScanCarry::default();
        Ok(())
    }
    /// Swap owner-bearing decoded storage after EOF or before resetting input.
    pub fn swap_decoded(&mut self, arena: &mut Vec<u8>) {
        core::mem::swap(&mut self.decoded, arena);
    }
    /// Select the sole duplicate-policy owner before consuming the input.
    /// SchemaVm ownership requires the caller to enforce duplicates itself.
    pub fn set_duplicate_owner(&mut self, owner: DuplicatePolicyOwner) -> Result<()> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        if self.byte_offset != 0 || !self.frames.is_empty() || self.pending.is_some() {
            return Err(syntax_error(
                ErrorCode::ConstraintViolation,
                self.byte_offset,
                "duplicate owner must be selected before parsing",
            ));
        }
        block_cursor::check_duplicate_owner(self.limits, owner)?;
        self.duplicate_owner = owner;
        Ok(())
    }
    pub const fn duplicate_owner(&self) -> DuplicatePolicyOwner {
        self.duplicate_owner
    }
    pub const fn limits(&self) -> Limits {
        self.limits
    }
    pub fn raw_bytes(&self) -> &[u8] {
        &self.input
    }
    pub fn decoded_bytes(&self) -> &[u8] {
        &self.decoded
    }
    pub const fn is_final(&self) -> bool {
        self.final_input
    }
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
    pub const fn fully_validated(&self) -> bool {
        self.finished
            && !self.query_skipped
            && !matches!(self.duplicate_owner, DuplicatePolicyOwner::SchemaVm)
    }
    /// Complete-input fast skip of the opening container most recently emitted.
    /// Incremental input continues to use the resumable event consumer.
    pub fn skip_open_container(&mut self, mode: jmax_abi::syntax::SkipMode) -> Result<Span32> {
        if let Some(error) = self.failure.as_ref() {
            return Err(error.clone());
        }
        let result = (|| {
            if !matches!(self.input, InputStorage::Complete(_)) {
                return Err(syntax_error(
                    ErrorCode::ConstraintViolation,
                    self.byte_offset,
                    "structural skip requires complete immutable input",
                ));
            }
            let frame = self.frames.last().ok_or_else(|| {
                syntax_error(
                    ErrorCode::TypeMismatch,
                    self.byte_offset,
                    "skip requires a current container start",
                )
            })?;
            if self.byte_offset != frame.start_offset + 1 || frame.count != 0 {
                return Err(syntax_error(
                    ErrorCode::TypeMismatch,
                    self.byte_offset,
                    "container has already been traversed",
                ));
            }
            let depth = (self.frames.len() - 1) as u16;
            let start = frame.start_offset;
            let cache = self.complete_cache.ok_or_else(|| {
                syntax_error(
                    ErrorCode::InternalError,
                    start,
                    "complete cursor cache is absent",
                )
            })?;
            let mut cursor = BlockCursor::from_cache(
                &self.input,
                self.limits,
                self.numeric,
                self.duplicate_owner,
                cache,
            );
            cursor.rewind_opening(start)?;
            cursor.discard = core::mem::take(&mut self.discard_scratch);
            let skipped = cursor.skip_value(&mut self.decoded, depth, mode);
            self.discard_scratch = core::mem::take(&mut cursor.discard);
            let span = skipped?;
            let cache = cursor.cache();
            let end = cursor.offset();
            drop(cursor);
            self.complete_cache = Some(cache);
            self.byte_offset = end;
            self.frames.pop();
            self.query_skipped |= matches!(mode, jmax_abi::syntax::SkipMode::QueryOnly);
            Ok(span)
        })();
        result.map_err(|error| self.fail(error))
    }
    pub fn failure(&self) -> Option<&Error> {
        self.failure.as_ref()
    }
    pub fn state(&self) -> CursorState {
        CursorState {
            byte_offset: self.byte_offset,
            depth: self.frames.len() as u16,
            phase: Some(self.frames.last().map_or(self.root_phase, |f| f.phase)),
            finished: self.finished,
        }
    }
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str> {
        let bytes = self.resolve_bytes(reference)?;
        core::str::from_utf8(bytes).ok()
    }
    pub(crate) fn resolve_bytes(&self, reference: StrRef) -> Option<&[u8]> {
        match reference.source {
            SpanSource::RawPayload => reference.span.slice(&self.input),
            SpanSource::DecodedArena => reference.span.slice(&self.decoded),
        }
    }

    /// Transfers backing storage without consuming or validating more events.
    /// The caller must retain its own record of whether traversal reached EOF.
    pub fn into_storage(self) -> Result<SyntaxOutput> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        Ok(SyntaxOutput {
            payload: self.input.into_payload(),
            decoded: self.decoded,
        })
    }
    pub(crate) fn fail(&mut self, error: Error) -> Error {
        self.failure.get_or_insert(error).clone()
    }
}

/// Syntax backing storage, without a syntax or schema validation claim.
/// `finish::into_output` validates before returning this type; `into_storage`
/// deliberately permits callers to retain a partially traversed query input.
#[derive(Clone, Debug)]
pub struct SyntaxOutput {
    pub payload: Payload,
    pub decoded: Vec<u8>,
}

pub(crate) const fn syntax_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Syntax, offset, detail)
}

/// Strict syntax validation of the exact borrowed input without copying it.
/// Scratch is cleared at entry and retains its allocation on both outcomes.
/// Unselected strings are validated without retaining their decoded output.
pub fn validate_borrowed(
    input: &[u8],
    limits: Limits,
    numeric: Option<jmax_abi::number::NumericPolicy>,
    scratch: &mut Scratch,
) -> Result<ValidationProof> {
    scratch.clear();
    let mut cursor = BlockCursor::new(input, limits, numeric, DuplicatePolicyOwner::Syntax)?;
    // The one-pass acceptor answers only "accepted", with the decoded buffer
    // left as the exact path leaves it. Anything else runs the exact path,
    // which alone produces diagnostics.
    if cursor.fast_validate(&mut scratch.decoded_buf).is_some() {
        // SAFETY: as below. The fast acceptor consumed the complete slice
        // under the same grammar, limits and numeric policy as the exact
        // traversal, and leaves the arena as that traversal does (empty).
        return Ok(unsafe {
            ValidationProof::issue_unchecked(
                0,
                input.len() as u32,
                scratch.decoded_buf.len() as u32,
            )
        });
    }
    cursor.skip_value(
        &mut scratch.decoded_buf,
        0,
        jmax_abi::syntax::SkipMode::StrictValidate,
    )?;
    cursor.finish_eof()?;
    // SAFETY: strict traversal checked all syntax, string references, UTF-8,
    // numeric policy and limits. No references escape the discard consumer;
    // scratch's arena is empty, and the complete original slice was consumed.
    Ok(unsafe {
        ValidationProof::issue_unchecked(0, input.len() as u32, scratch.decoded_buf.len() as u32)
    })
}

/// Strictly validates a complete borrowed input while reusing both the decoded
/// arena and strict-discard tables. The tables retain capacity for later calls.
pub fn validate_borrowed_reusing(
    input: &[u8],
    limits: Limits,
    numeric: Option<jmax_abi::number::NumericPolicy>,
    scratch: &mut Scratch,
    skip_scratch: &mut SkipScratch,
) -> Result<ValidationProof> {
    scratch.clear();
    let mut cursor = BlockCursor::with_scratch(
        input,
        limits,
        numeric,
        DuplicatePolicyOwner::Syntax,
        skip_scratch,
    )?;
    if cursor.fast_validate(&mut scratch.decoded_buf).is_some() {
        // SAFETY: the fast acceptor checked the complete input under the same
        // policy as the exact traversal and leaves the decoded arena empty.
        return Ok(unsafe {
            ValidationProof::issue_unchecked(
                0,
                input.len() as u32,
                scratch.decoded_buf.len() as u32,
            )
        });
    }
    cursor.skip_value(
        &mut scratch.decoded_buf,
        0,
        jmax_abi::syntax::SkipMode::StrictValidate,
    )?;
    cursor.finish_eof()?;
    // SAFETY: the exact strict traversal consumed the entire input and retained
    // no references in scratch.
    Ok(unsafe {
        ValidationProof::issue_unchecked(0, input.len() as u32, scratch.decoded_buf.len() as u32)
    })
}

/// Lifetime-bound syntax capability. Keeping this witness alive retains the
/// immutable borrow, preventing mutation/reallocation of its input in safe Rust.
/// The unbound ABI token is intentionally not exposed by a safe accessor.
#[derive(Debug)]
pub struct BorrowedValidation<'a> {
    input: &'a [u8],
    limits: Limits,
    proof: ValidationProof,
    capability: ValidationCapability,
}
impl<'a> BorrowedValidation<'a> {
    pub const fn input(&self) -> &'a [u8] {
        self.input
    }
    pub const fn proof(&self) -> ValidationProof {
        self.proof
    }
    pub const fn limits(&self) -> Limits {
        self.limits
    }
    /// Executes a boundary skip while this witness keeps the input borrowed.
    pub fn skip_value(
        &self,
        cursor: &mut BlockCursor<'a>,
        decoded: &mut Vec<u8>,
        depth: u16,
    ) -> Result<Span32> {
        cursor.skip_value(
            decoded,
            depth,
            jmax_abi::syntax::SkipMode::ValidatedOwner(self.capability),
        )
    }
    pub fn skip_open_container(
        &self,
        cursor: &mut BlockCursor<'a>,
        decoded: &mut Vec<u8>,
    ) -> Result<Span32> {
        cursor.skip_open_container(
            decoded,
            jmax_abi::syntax::SkipMode::ValidatedOwner(self.capability),
        )
    }
}
/// Issues a borrow-retaining capability after strict syntax validation. Numeric
/// policies need a distinct fingerprint and are deliberately excluded here.
pub fn validate_borrowed_with_capability<'a>(
    input: &'a [u8],
    limits: Limits,
    scratch: &mut Scratch,
) -> Result<BorrowedValidation<'a>> {
    let proof = validate_borrowed(input, limits, None, scratch)?;
    // SAFETY: validation above covered precisely this immutable borrowed range
    // under syntax schema 0 and all fingerprinted limits. The returned witness
    // retains that immutable borrow for every safe use of this capability.
    let capability = unsafe {
        ValidationCapability::issue_unchecked(
            input.as_ptr() as usize,
            input.as_ptr() as usize,
            input.len() as u32,
            proof.decoded_len(),
            0,
            limits.policy_fingerprint(),
        )
    };
    Ok(BorrowedValidation {
        input,
        limits,
        proof,
        capability,
    })
}
