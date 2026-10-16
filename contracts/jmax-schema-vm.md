# jmax-schema-vm frozen contract, version 1

The root snapshot below is normative. Later modules import super::* and must not redeclare ABI types. Allowed dependencies are jmax-abi, jmax-schema-ir, jmax-schema-compile, jmax-cursor, jmax-syntax, jmax-format, jmax-decimal and jmax-buffer. This crate is synchronous, no_std plus alloc. All caller-controlled spans, IDs, lengths and counters require checked access. A ValidatedProgram witness is mandatory at the execution boundary.

## Wave 1: execute.rs

```rust
pub fn run<I: EventInput>(program: &ValidatedProgram, input: I, selection: &Selection, limits: VmLimits) -> Result<ValidatedRun>;
pub fn validate<I: EventInput>(program: &ValidatedProgram, input: I, selection: &Selection, limits: VmLimits) -> Result<ValidatedSlots>;
pub(crate) fn step(context: &mut VmContext<'_>, event: Event, raw: &[u8], decoded: &[u8]) -> Result<()>;
```

run checks payload/arena lengths fit u32 and syntax limits, validates selection, creates bounded scratch, and drives every event until next_event returns None. None must come from the strict EventInput implementation after full grammar/trailing-content checks, not merely after the root container closes. validate is extract::publish(run(...)). Keep VmContext, its completion flags and ValidatedRun construction private. EventInput is an unsafe extension boundary with the exact safety obligations in the root: independent safe callers cannot supply arbitrary events and obtain a proof. This crate does not invent an unchecked event adapter; downstream cursor/syntax adapters must audit their unsafe impl against those obligations.

Enforce event structure defensively: one root, matching closes, keys only in objects, one key per value, no missing values, valid scalar metadata, no unfinished frames and no trailing events. Events are interpreted with the current ProgramState's validated instruction template, not by falling through unrelated ranges. The schema's graph may recurse, but execution uses a bounded explicit Vec stack. Increment array/member counts once per value/member; opening a child does not increment them again when it closes. Apply shape constraints on all fields regardless of selection. For unknown/unconstrained containers use state=None and still enforce syntax, depth/count/string/number/duplicate/resource limits. Reject unknowns if either compiled object policy or runtime reject_unknown_keys requires it. For state=None there is no known-key whitelist, so runtime reject_unknown_keys alone does not reject every key.

Use FrameKind/ObjectState/VmFrame for frames. A frame's start is its opening brace/bracket byte. Closing offset denotes the closing delimiter's byte position, so a captured span ends at checked offset+1. Root scalars complete immediately; containers complete only on their matching close. A parent ObjectState.pending is consumed at value start; a newly opened child retains field_id/slot until completion. Array frames hold the repeated item StateId or None for unconstrained arrays. A pending slot is written only after that value and all its constraints have succeeded.

Empty selection.field_ids means capture all compiled slots; a nonempty list selects those FieldIds, leaving all other slots Empty. The output vector always has header.slot_count elements. check_selection rejects duplicates, unknown/no-slot IDs, and strict_validate_unselected=false. Before calling it, run bounds selection length by the slot count and charges an overflow-checked upper bound for its comparisons against the VM step budget. All slots, frames, seen masks and retained unknown names count against max_scratch_bytes. Charge at least one step per event, instruction, key byte/hash/comparison work, required-bit scan, number digit and string scalar; regex execution additionally consumes the SAME remaining budget. Never reset a budget per field. All allocation is fallible and accounted before retaining scratch; release memory accounting when frames are destroyed. Arena/payload storage belongs to input limits, not VM scratch. Work and scratch arithmetic must be checked.

After successful EOF and empty stack, consume into_buffers and verify its lengths and ownership assumptions against the buffers used by the run. EventInput guarantees exact immutable owners, not merely equal lengths; a dishonest unsafe implementation is outside the safe contract. Revalidate all published slot ranges, string UTF-8/scalar counts, raw spans and byte lengths against the returned owners. Only then construct ValidatedRun with slots, buffers and the program header fingerprint. No run is published on any error, including into_buffers failure. Unselected fields are still fully checked.

## Wave 2: objects.rs

```rust
pub fn begin(program: &ValidatedProgram, state: Option<StateId>, budget: &mut VmBudget, offset: u32) -> Result<ObjectState>;
pub fn key(program: &ValidatedProgram, object: &mut ObjectState, key: &str, selection: &Selection, limits: &VmLimits, budget: &mut VmBudget, offset: u32) -> Result<PendingValue>;
pub fn take_value(object: &mut ObjectState, offset: u32) -> Result<PendingValue>;
pub fn finish(program: &ValidatedProgram, state: Option<StateId>, object: &ObjectState, budget: &mut VmBudget, offset: u32) -> Result<()>;
```

begin accepts an Object state, Any state, or None; other kinds -> TypeMismatch. An Object allocates ceil(key_count/64) zeroed seen words using checked arithmetic. Any/None sets table=None and tracks all names as unknown. A state's key_table must exist in the validated program. No required mask is stored globally: recursive instances each get independent seen state.

key requires no outstanding pending value. Increment property_count with overflow/limit checks. Match decoded key bytes using compile::keys::lookup. Known keys use their entry's index within the table as the seen-bit index, including OPTIONAL keys; required_bit is a distinct dense required-field numbering. Mark known seen bits. A repeated known key is DuplicateObjectKey when reject_duplicate_keys=true; if false and the key has a compiled slot, reject repeated capture with ConstraintViolation rather than overwrite. For unknown keys, retain exact decoded bytes when duplicate detection is required, bounded by scratch/count/work budgets; names with different escape spellings compare equal after decoding. Unknown keys fail immediately when policy requires rejection. Otherwise their PendingValue has state=None, slot=None, field_id=FieldId(0).

For known keys return/store a PendingValue with target StateId, actual FieldId and selected slot if any. An empty selection captures all; unknown/no-slot fields cannot be selected. take_value removes and returns pending, failing InvalidSyntax if absent. finish requires no pending value, checks every required key's seen bit and property-count bounds, and reports MissingRequiredField with the missing field's ID. Required checks must run even if duplicate/unknown rejection is disabled or no fields were selected. Wrong state/kind -> TypeMismatch; counter/field limits -> BudgetExhausted, malformed event ordering -> InvalidSyntax.

## Wave 3: constraints.rs

```rust
pub fn check_scalar(program: &ValidatedProgram, state: Option<StateId>, value: ScalarInput<'_>, budget: &mut VmBudget) -> Result<SlotValue>;
pub fn check_count(constraints: &ConstraintSet, count: u32, offset: u32) -> Result<()>;
pub fn check_pattern(program: &PatternProgram, text: &str, budget: &mut VmBudget, offset: u32) -> Result<()>;
```

ScalarInput.span is the complete raw token (including quotes for String). For strings decoded must be Some and resolve_str must succeed; for other kinds decoded is None. StrRef points only at decoded contents (raw source is valid for an unescaped interior). Raw token ranges are bounds checked; token kind, lexical form and referenced string must agree with the audited EventInput. Null maps to RawSpan of its token, never Empty; Empty is exclusively absent/unselected. True/False map to Bool. An unconstrained number maps to U64/I64 when integral and representable, otherwise F64Bits after strict conversion; a fraction/exponent always takes the floating path. Strings map to Str, preserving owner-bound StrRef.

ValueKind mapping: Null -> Null, Boolean -> either boolean, Integer -> a JSON Number containing integer grammar only (no fraction/exponent), Number -> JSON Number converted to F64Bits, String -> JSON String. Decimal -> JSON String containing fixed-point numeric text, converted to Decimal128 according to its DecimalPolicy; unquoted JSON numbers do not satisfy Decimal in version 1. numeric.allow_Quoted_int additionally permits Integer from decoded string contents with the identical strict integer grammar. Any/None accept all scalar categories. Object/Array reject scalars with TypeMismatch.

Integer values use u64 for nonnegative or i64 for negative. Compare optional i64 bounds without narrowing a large u64 (use checked/widened comparison), then return the natural U64/I64 slot. Strict lexical validation forbids leading plus/zeros, incomplete exponent/fraction, whitespace and nonnumeric suffixes. NumericPolicy applies to all number conversion including Any: reject negative zero when configured, enforce max_exponent_abs with checked exponent parsing, and reject nonfinite floating results when configured. Overflow is NumberOverflow, malformed numeric text is InvalidNumber. Decimal parsing enforces precision, target scale, exponent policy, exact rescaling when requested, min/max and positive tick step; never round silently when reject_inexact_rescale is true. Use the canonical ABI Decimal128/DecimalPolicy and allowed decimal dependency, with no float intermediary. Precision/scale/tick violations use the matching ABI codes; decimal bound failures use ConstraintViolation.

String lengths use decoded Unicode scalar count; runtime max_string_bytes uses decoded UTF-8 bytes. Apply min_count/max_count, then compiled pattern and format validators. check_pattern calls patterns::is_match with budget.steps_left, mapping false to PatternMismatch and preserving BudgetExhausted. Format dispatch uses the compiled program's format_options, caps work/input length against the shared budget, and maps failures through FormatFailure::into_error(raw token offset, SchemaVm). Never add decoded error indices to raw offsets. check_count is inclusive and returns ConstraintViolation for either failed bound. Apply it on object/array close and string validation. No arithmetic, conversion, regex or format error can be treated as a successful skip.

## Wave 4: extract.rs

```rust
pub fn selected(selection: &Selection, field: FieldId) -> bool;
pub fn store(slots: &mut [SlotValue], slot: u32, value: SlotValue, offset: u32) -> Result<()>;
pub fn publish(run: ValidatedRun) -> Result<ValidatedSlots>;
```

selected returns true for an empty field_ids list or exact membership. store checks the slot range and that its current value is Empty, rejects incoming Empty and repeat writes with ConstraintViolation, and never partially changes a slot on error. Invalid slot index -> InternalError. A compiled captured field may not repeat, even when unselected; compiler restrictions plus execution's seen logic enforce this.

publish consumes the private completed-run capability. Recheck payload/decoded u32 lengths and all slot spans/string references before the sole small audited unsafe block issuing ValidationProof::issue_unchecked(fingerprint,payload_len,decoded_len). Assemble ABI ValidatedSlots with those EXACT owners and slots. A proof does not originate from user-provided ValidatedSlots, a caller-supplied fingerprint, events alone, or a schema image without its validation witness. Do not expose a public constructor for ValidatedRun. Failures leave no externally visible proof. ABI ValidatedSlots fields are public, so downstream code must not assume a mutated instance still binds the same owners merely because it contains a proof.

## Errors and cross-module invariants

Use SchemaVm for errors originating here and preserve dependency errors when they already identify the failure precisely. Attach current schema and known FieldId at the interpreter boundary. Offsets are raw payload token positions; decoded source offsets must never be reported as raw positions. Missing required fields and count violations use closing delimiter offsets. Bad syntax/EOF/trailing data use their matching ABI codes. Depth/payload/arena limits use DepthLimitExceeded/PayloadTooLarge/DecodedArenaOverflow; work, scratch, count limits and allocation failure use BudgetExhausted. Constraint failures, patterns and formats retain their distinct ABI codes. Invalid internal indices in a witnessed program use InternalError, not panics.

EventInput::next_event error terminates the run immediately. VmBudget::charge/retain never replenish resources; release only removes already-accounted retained bytes and rejects underflow. max_depth is inclusive; root scalar depth is zero and a root container is one. A proof always covers the whole strict JSON document and every schema constraint, independent of extraction. Query-only operation must use a separate API without ValidationProof.

## Normative shared root

```rust
//! Schema validation state and the ownership boundary for publishing ABI proofs.
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
pub use jmax_abi::buffer::{Payload, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{ConvertedNumber, Decimal128};
pub use jmax_abi::schema::{FieldId, Selection, SlotValue, StateId, ValidatedSlots};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::{Event, Limits, ScalarKind};
pub use jmax_schema_compile::{CompiledProgram, ConstraintSet, KeyTable, PatternProgram,
    ProgramState, SlotSpec, StateKind, ValidatedProgram};
pub use jmax_schema_ir::ValueKind;

pub mod execute;
pub mod objects;
pub mod constraints;
pub mod extract;

pub const DEFAULT_MAX_VM_STEPS: u64 = 16_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmLimits {
    pub syntax: Limits,
    pub max_steps: u64,
    /// Includes seen masks, retained unknown keys, frames and slot scratch.
    pub max_scratch_bytes: u32,
}

impl Default for VmLimits {
    fn default() -> Self {
        Self { syntax: Limits::strict_default(), max_steps: DEFAULT_MAX_VM_STEPS,
            max_scratch_bytes: 16 * 1_024 * 1_024 }
    }
}

#[derive(Clone, Debug)]
pub struct InputBuffers { pub payload: Payload, pub decoded: Box<[u8]> }

#[derive(Clone, Copy, Debug)]
pub struct ScalarInput<'a> {
    pub kind: ScalarKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
    pub raw: &'a [u8],
    pub arena: &'a [u8],
}

/// A complete, strictly parsed JSON event source tied to immutable owned buffers.
///
/// # Safety
/// Implementors must validate UTF-8 and the complete JSON grammar, emit every event
/// in order exactly once, and report EOF only after one root and no trailing data.
/// Every span and decoded scalar count must describe those exact buffers. Buffers
/// must remain byte-for-byte unchanged, including through `into_buffers`, and that
/// method must return the owners exposed by `raw_bytes` and `decoded_bytes`.
/// Syntax failures, duplicate policy and all supplied limits must be enforced by
/// the VM or source before publication. An event adapter must never trust caller
/// supplied events or a proof for a different owner to establish these guarantees.
pub unsafe trait EventInput: Sized {
    fn raw_bytes(&self) -> &[u8];
    fn decoded_bytes(&self) -> &[u8];
    fn next_event(&mut self) -> Result<Option<Event>>;
    fn into_buffers(self) -> Result<InputBuffers>;
}

/// Extraction cannot weaken validation. False is a query-only request and is rejected.
pub fn check_selection(program: &ValidatedProgram, selection: &Selection) -> Result<()> {
    if !selection.strict_validate_unselected {
        return Err(vm_error(ErrorCode::UnsupportedSchemaFeature, 0,
            "proof publication requires validation of unselected fields")
            .with_schema(program.image().header.schema_id, 0));
    }
    for (index, field) in selection.field_ids.iter().enumerate() {
        if selection.field_ids[..index].contains(field) {
            return Err(vm_error(ErrorCode::ConstraintViolation, 0,
                "selection contains a duplicate field")
                .with_schema(program.image().header.schema_id, field.0));
        }
        if !program.program().slots.iter().any(|slot| slot.field_id == *field) {
            return Err(vm_error(ErrorCode::UnknownObjectKey, 0,
                "selection field has no compiled slot").with_schema(
                    program.image().header.schema_id, field.0));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerKind { Object, Array }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingValue {
    /// None is an unconstrained unknown field.
    pub state: Option<StateId>,
    pub field_id: FieldId,
    pub slot: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ObjectState {
    pub table: Option<u32>,
    pub seen_words: Vec<u64>,
    pub unknown_keys: Vec<Vec<u8>>,
    pub property_count: u32,
    pub pending: Option<PendingValue>,
}

#[derive(Clone, Debug)]
pub enum FrameKind {
    Object(ObjectState),
    Array { count: u32, items: Option<StateId> },
}

#[derive(Clone, Debug)]
pub struct VmFrame {
    pub state: Option<StateId>,
    pub field_id: FieldId,
    pub start: u32,
    pub slot: Option<u32>,
    pub kind: FrameKind,
}

/// A completed run is a private capability, not a collection of caller assertions.
#[derive(Debug)]
pub struct ValidatedRun {
    pub(crate) buffers: InputBuffers,
    pub(crate) slots: Vec<SlotValue>,
    pub(crate) fingerprint: u64,
}

impl ValidatedRun {
    pub fn slots(&self) -> &[SlotValue] { &self.slots }
    pub fn payload(&self) -> &Payload { &self.buffers.payload }
    pub fn decoded_bytes(&self) -> &[u8] { &self.buffers.decoded }
    pub fn schema_fingerprint(&self) -> u64 { self.fingerprint }
}

pub(crate) struct VmContext<'a> {
    pub program: &'a ValidatedProgram,
    pub selection: &'a Selection,
    pub limits: VmLimits,
    pub frames: Vec<VmFrame>,
    pub slots: Vec<SlotValue>,
    pub budget: VmBudget,
    pub root_started: bool,
    pub root_done: bool,
}

#[derive(Clone, Debug)]
pub struct VmBudget {
    pub(crate) steps_left: u64,
    pub(crate) scratch_bytes: u32,
    pub(crate) max_scratch_bytes: u32,
}

impl VmBudget {
    pub const fn new(limits: VmLimits) -> Self {
        Self { steps_left: limits.max_steps, scratch_bytes: 0,
            max_scratch_bytes: limits.max_scratch_bytes }
    }
    pub const fn steps_left(&self) -> u64 { self.steps_left }
    pub const fn scratch_bytes(&self) -> u32 { self.scratch_bytes }

    pub fn charge(&mut self, units: u64, offset: u32) -> Result<()> {
        self.steps_left = self.steps_left.checked_sub(units).ok_or_else(||
            vm_error(ErrorCode::BudgetExhausted, offset, "schema VM step budget exhausted"))?;
        Ok(())
    }

    pub fn retain(&mut self, bytes: u32, offset: u32) -> Result<()> {
        let next = self.scratch_bytes.checked_add(bytes).ok_or_else(||
            vm_error(ErrorCode::BudgetExhausted, offset, "schema VM scratch size overflow"))?;
        if next > self.max_scratch_bytes {
            return Err(vm_error(ErrorCode::BudgetExhausted, offset,
                "schema VM scratch limit exceeded"));
        }
        self.scratch_bytes = next;
        Ok(())
    }

    pub fn release(&mut self, bytes: u32) -> Result<()> {
        self.scratch_bytes = self.scratch_bytes.checked_sub(bytes).ok_or_else(||
            vm_error(ErrorCode::InternalError, 0, "schema VM scratch accounting underflow"))?;
        Ok(())
    }
}

/// Resolve a string with checked ranges, UTF-8 validation and exact scalar count.
pub fn resolve_str<'a>(raw: &'a [u8], decoded: &'a [u8], value: StrRef) -> Result<&'a str> {
    let (owner, error_offset) = match value.source {
        SpanSource::RawPayload => (raw, value.span.offset),
        SpanSource::DecodedArena => (decoded, 0),
    };
    let bytes = value.span.slice(owner).ok_or_else(||
        vm_error(ErrorCode::InternalError, error_offset, "string span is out of bounds"))?;
    let text = core::str::from_utf8(bytes).map_err(|_|
        vm_error(ErrorCode::InvalidUtf8, 0, "event string is not valid UTF-8"))?;
    if text.chars().count() != value.scalar_len as usize {
        return Err(vm_error(ErrorCode::InternalError, 0, "incorrect string scalar count"));
    }
    Ok(text)
}

pub(crate) fn vm_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaVm, offset, detail)
}
```

## budget_safe

```rust
pub struct BudgetBound { pub steps: u64, pub scratch_bytes: u64 }
pub fn budget_bound(program: &ValidatedProgram, len: usize, limits: VmLimits) -> Option<BudgetBound>;
pub fn budget_safe(program: &ValidatedProgram, len: usize, limits: VmLimits) -> bool;
pub fn budget_bound_counted(program: &ValidatedProgram, len: usize, decimals: u64, limits: VmLimits) -> Option<BudgetBound>;
pub fn budget_safe_counted(program: &ValidatedProgram, len: usize, decimals: u64, limits: VmLimits) -> bool;
```

budget_bound returns an upper bound, valid for every input of at most len bytes, on the VM
steps and scratch bytes that a strict validate_borrowed_into can charge, as a function of len,
the program and limits.syntax. It is deliberately conservative and returns None when no bound is
derived: any program state with a pattern constraint, an object that retains unknown keys, a constrained untyped value, a program with an unbounded
construct, or an arithmetic overflow. budget_safe is true only when budget_bound is Some, its
steps do not exceed limits.max_steps, its scratch bytes do not exceed limits.max_scratch_bytes,
len does not exceed limits.syntax.max_payload_bytes or limits.syntax.max_decoded_bytes, and len
fits u32. When true, the VM cannot fail with BudgetExhausted, DecodedArenaOverflow or
PayloadTooLarge for this program on any such input. A test measures real consumption
(by bisecting max_steps and max_scratch_bytes) over the corpora and their mutations and asserts
it never exceeds the bound. Both functions are pure and allocation free.

The VM bills every decimal scalar a fixed 2,048 steps (a prepayment for rescaling and
validation) whatever its length, so a bound that knows only len must assume the densest
possible decimals (at most len / 4 + 1 of them) and cannot vouch for a long book of decimals.
budget_bound_counted takes the number of decimal scalars the input holds and charges
`len * per_byte + fixed + decimals * 2,096` (the 2,096 is the 2,048 plus slack for the
decimal's own passes; `per_byte` is the bound's byte factor without the decimal term, and
the decimal term is added only to programs that have a decimal state). budget_bound is
budget_bound_counted with decimals = len / 4 + 1, and budget_safe is budget_safe_counted
with that count,, though their bound is slightly larger than before for programs with decimals. A caller may pass a count it has verified,
for example the number of decimals a native decoder has just read: an input holding at most
that many decimal scalars cannot exhaust the budget when budget_safe_counted is true. The
test measures the counted bound the same way, over 1,000 level books, one-digit decimals
and long, escaped, signed and exponent decimals, with measured/bound at most 0.84.
