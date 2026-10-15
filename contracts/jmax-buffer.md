# jmax-buffer frozen contract

Owner: W0-ROOTS-1. This contract freezes the root vocabulary and the public APIs
implemented by Waves 1 through 4. Only `jmax-abi` is an allowed crate dependency.
Use `alloc` and `core`; support `no_std` with an allocator. Do not define a second
Payload, Span32, StrRef, ParsedMessage, or ValidationProof.

## Root exports and definitions

The root declares `pub mod payload; pub mod arena; pub mod views; pub mod retention;`.
Every child may `use super::*`. Root-private imports available to children are
`alloc::{boxed::Box, sync::Arc, vec::Vec}` and `core::ops::Range`.
The canonical reexports are:

```rust
pub use jmax_abi::buffer::{ParsedMessage, Payload, UnvalidatedSpan, ValidationProof};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};

pub const MAX_BUFFER_BYTES: usize = u32::MAX as usize;
pub const DEFAULT_MAX_PAYLOAD_BYTES: u32 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_DECODED_BYTES: u32 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferLimits {
    pub max_payload_bytes: u32,
    pub max_decoded_bytes: u32,
}
#[derive(Clone, Debug)]
pub struct DecodedArena {
    pub(crate) bytes: Vec<u8>,
    pub(crate) limit: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct OwnerBytes<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) decoded: &'a [u8],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteView<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) span: Span32,
    pub(crate) source: SpanSource,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrView<'a> {
    pub(crate) text: &'a str,
    pub(crate) reference: StrRef,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingSize { Exact(u64), AtLeast(u64) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionStats {
    pub payload_visible_bytes: u64,
    pub payload_backing_bytes: BackingSize,
    pub decoded_len: u64,
    pub decoded_capacity: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_visible_bytes: u32,
    pub min_savings_bytes: u32,
    pub copy_unknown_backing: bool,
}
#[derive(Clone, Debug)]
pub struct RetainedRange {
    pub(crate) payload: Payload,
    pub(crate) original: Span32,
    pub(crate) retained: Span32,
    pub(crate) compacted: bool,
}
```

Root-implemented inherent methods and functions, with exact signatures:

```rust
impl Default for BufferLimits { fn default() -> Self; }
impl<'a> ByteView<'a> {
    pub const fn as_bytes(&self) -> &'a [u8];
    pub const fn span(&self) -> Span32;
    pub const fn source(&self) -> SpanSource;
    pub const fn len(&self) -> usize;
    pub const fn is_empty(&self) -> bool;
}
impl<'a> StrView<'a> {
    pub const fn as_str(&self) -> &'a str;
    pub const fn reference(&self) -> StrRef;
    pub const fn len(&self) -> usize;
    pub const fn is_empty(&self) -> bool;
}
impl BackingSize {
    pub const fn minimum(self) -> u64;
    pub const fn exact(self) -> Option<u64>;
}
impl RetentionStats {
    pub const fn minimum_retained_bytes(&self) -> u64;
    pub const fn exact_retained_bytes(&self) -> Option<u64>;
}
impl Default for RetentionPolicy { fn default() -> Self; }
pub fn checked_span(offset: usize, len: usize, source: SpanSource) -> Result<Span32>;
pub fn checked_range(span: Span32, buffer_len: usize) -> Result<Range<usize>>;
```

BufferLimits defaults to the two root byte limits. RetentionPolicy defaults to
`{ max_visible_bytes: DEFAULT_MAX_PAYLOAD_BYTES, min_savings_bytes: 4096,
copy_unknown_backing: true }`. StrView length is UTF-8 bytes, not scalar count.
BackingSize::minimum returns either payload; exact returns Some only for Exact.
The statistics total payload backing plus decoded capacity. Minimum addition
saturates; exact addition returns None on overflow or unknown backing.

`checked_span` rejects unrepresentable offset, length, or offset + length. Its
error is PayloadTooLarge for RawPayload or DecodedArenaOverflow for DecodedArena,
source Decode, offset u32::MAX, detail `buffer span exceeds ABI range`.
`checked_range` rejects overflowing span ends or end > buffer_len with
InvalidToken/Decode, span.offset, `buffer span is out of bounds`. Empty spans at
buffer_len are valid. It does not require the entire buffer_len to fit the ABI.

## Wave 1: src/payload.rs

```rust
pub fn adopt(payload: Payload, limits: BufferLimits) -> Result<Payload>;
pub fn from_arc(bytes: Arc<[u8]>, limits: BufferLimits) -> Result<Payload>;
pub fn from_vec(bytes: Vec<u8>, limits: BufferLimits) -> Result<Payload>;
pub fn from_slice(bytes: &[u8], limits: BufferLimits) -> Result<Payload>;
pub fn validate_len(len: usize, limits: BufferLimits) -> Result<u32>;
```

All paths check len <= u32::MAX and len <= limits.max_payload_bytes before copying
or adopting. Equality and zero-length payloads are valid, including a zero limit.
On violation return PayloadTooLarge/Decode, offset u32::MAX, static detail.
Adopt returns the same Payload variant without copying or changing its visible
range. From_arc uses Payload::Shared without copying the byte allocation.
From_vec transfers ownership through Payload::from_vec; no byte-by-byte copy is
required and no promise is made about capacity preservation. From_slice copies.
Bytes adoption is deliberately `adopt(Payload::Bytes(bytes), limits)` using the
canonical ABI variant. Do not import or reexport the external bytes crate.
These functions perform no UTF-8, JSON, or schema validation.

## Wave 2: src/arena.rs

Implement these inherent methods on the root DecodedArena, not another type:

```rust
impl DecodedArena {
    pub fn new(max_decoded_bytes: u32) -> Self;
    pub fn with_capacity(capacity: usize, max_decoded_bytes: u32) -> Result<Self>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn capacity(&self) -> usize;
    pub fn limit(&self) -> u32;
    pub fn as_slice(&self) -> &[u8];
    pub fn reserve(&mut self, additional: usize) -> Result<()>;
    pub fn append(&mut self, bytes: &[u8]) -> Result<Span32>;
    pub fn append_str(&mut self, text: &str) -> Result<StrRef>;
    pub fn truncate(&mut self, len: u32) -> Result<()>;
    pub fn clear(&mut self);
    pub fn into_boxed_slice(self) -> Box<[u8]>;
}
impl Default for DecodedArena { fn default() -> Self; }
```

Default uses DEFAULT_MAX_DECODED_BYTES. New allocates nothing. Logical byte length
never exceeds limit. With_capacity checks capacity <= limit. Reserve checks
len + additional with checked arithmetic against limit, then uses try_reserve;
allocator rounding may produce capacity above limit. Append reserves first and
returns the appended region with source-independent Span32 coordinates.
Append_str returns StrRef { span, source: DecodedArena, scalar_len }, where
scalar_len is text.chars().count() checked before mutation. Append permits any
bytes; the arena as a whole need not be UTF-8. Truncate refuses len > current len.
Clear and truncate retain allocation and invalidate spans into removed bytes.
Into_boxed_slice transfers exactly the logical bytes, excluding spare capacity.

Length overflow or limit violation: DecodedArenaOverflow/Decode at the current
arena length (with_capacity uses offset 0). Allocation failure from try_reserve:
BudgetExhausted/Decode at current length. Invalid truncation: InvalidToken/Decode
at requested len. Failed mutations preserve logical bytes and length; capacity
may change. No unchecked conversions or span arithmetic.

## Wave 3: src/views.rs

```rust
impl<'a> OwnerBytes<'a> {
    pub fn new(raw: &'a [u8], decoded: &'a [u8]) -> Result<Self>;
    pub fn from_message<T>(message: &'a ParsedMessage<T>) -> Result<Self>;
    pub fn raw_bytes(&self) -> &'a [u8];
    pub fn decoded_bytes(&self) -> &'a [u8];
    pub fn view(&self, source: SpanSource, span: Span32) -> Result<ByteView<'a>>;
    pub fn string(&self, reference: StrRef) -> Result<StrView<'a>>;
}
```

New checks both owner lengths fit u32, using PayloadTooLarge or
DecodedArenaOverflow, source Decode, offset u32::MAX. From_message delegates to
new(message.raw_bytes(), message.decoded_bytes()); it neither trusts nor issues a
proof. View chooses exactly the declared source, checks checked_range, and binds
the resulting borrow to the owner's lifetime. String additionally validates UTF-8
and exact Unicode scalar count, even if a message has a proof. Invalid UTF-8:
InvalidUtf8/Decode at span.offset + valid_up_to. Count mismatch:
ConstraintViolation/Decode at span.offset. Empty strings require scalar_len = 0.
StrRef carries no owner identity: the caller chooses which owner to resolve
against. Views guarantee bounds, UTF-8 where applicable, and borrow lifetime;
they do not authenticate a reference's provenance or validate JSON escapes.

## Wave 4: src/retention.rs

```rust
pub fn account(payload: &Payload, decoded: &DecodedArena) -> RetentionStats;
pub fn retain_span(
    payload: &Payload, span: Span32, policy: RetentionPolicy,
) -> Result<RetainedRange>;
impl RetainedRange {
    pub fn payload(&self) -> &Payload;
    pub fn original_span(&self) -> Span32;
    pub fn span(&self) -> Span32;
    pub fn is_compacted(&self) -> bool;
    pub fn as_slice(&self) -> &[u8];
    pub fn into_payload(self) -> Payload;
}
```

Account reports visible Payload len, arena len and Vec capacity. Shared backing
is Exact(Arc slice len); Bytes backing is AtLeast(visible len), including empty
Bytes. Counts exclude Arc control blocks, allocator overhead, and other owners;
this is retained backing attributable to this owner, not exclusive ownership or
process resident memory. Never claim a Bytes backing size is exact.

Retain_span first checks the payload fits u32 and validates span with checked_range.
Payload length overflow uses PayloadTooLarge/Decode at u32::MAX. If span.len
exceeds max_visible_bytes, return BudgetExhausted/Decode at span.offset. Otherwise
copy when any of these hold: whole visible payload length exceeds the budget;
positive visible savings (payload.len - span.len) are >= min_savings_bytes; backing
is unknown and copy_unknown_backing is true. Copy only the selected bytes into
Payload::Shared(Arc<[u8]>), set retained = Span32::new(0, span.len), compacted =
true. This also makes a zero-length result detach from any large backing. If no
copy is needed, clone the original Payload, preserve its variant and visible
range, retained = original = span, compacted = false. Original always records the
input coordinates. As_slice resolves retained against the privately stored
owner. Into_payload returns that whole owner; use span to select the range when
the result was not compacted.

The budget is a hard bound on visible retained bytes, not an unknowable Bytes
backing size. With copy_unknown_backing=true, copied unknown backings become
known exact slice lengths. Retention must never mutate ParsedMessage or reuse
its proof for rebased spans; it returns an independent range without a proof.

## Shared invariants and integration

All errors are canonical ABI Error with static, payload-free detail. No unsafe
code, transmutation, leaked storage, forged proof, or lifetime extension. Checked
views and retention are read-only with respect to input. Infallible ABI ownership
constructors, Clone, Arc conversion, and into_boxed_slice follow alloc's normal
allocation-failure behavior; only explicit try_reserve paths promise an ABI
BudgetExhausted result. The arena's logical limit is not an allocation-capacity
limit. Root definitions must not be redeclared in wave modules.

Dependency direction: payload is independent; arena uses root helpers; views uses
root types and helpers; retention uses arena's public accessors and root helpers.
No wave may depend on a later wave. No schema validation or proof issuance is
performed anywhere in this crate.
