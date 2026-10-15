# jmax-utf8: frozen Wave 0 contract

Status: frozen for Waves 1 through 4. The root owns the shared definitions.
Required dependency: `jmax-abi`. The existing `simd-utf8` feature enables the
already declared optional `ps-simdutf8` integration requested for Wave 1; no
manifest change or new dependency is authorized. All APIs are safe and perform
no allocation, including no-std builds. The Payload-borrowing entry point is
available only with alloc; all other entry points support no-alloc builds.
Unsafe code is forbidden.

## Canonical ABI and exact root definitions

```rust
#[cfg(feature = "alloc")]
pub use jmax_abi::buffer::Payload;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BlockMasks, ScanCarry};
pub use jmax_abi::span::Span32;
pub const MAX_SEQUENCE_BYTES: usize = 4;
pub const MAX_PENDING_BYTES: usize = MAX_SEQUENCE_BYTES - 1;
pub const BLOCK_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Utf8Summary {
    pub byte_len: u32,
    pub scalar_len: u32,
    pub ascii_only: bool,
}
impl Default for Utf8Summary { fn default() -> Self; }

#[derive(Clone, Debug)]
pub struct Utf8Stream {
    carry: ScanCarry,
    total_bytes: u32,
    total_scalars: u32,
    ascii_only: bool,
    finished: bool,
    failure: Option<Error>,
}
impl Utf8Stream {
    pub fn new() -> Self;
    pub fn reset(&mut self);
    pub const fn bytes_seen(&self) -> u32;
    pub const fn scalars_seen(&self) -> u32;
    pub fn pending_bytes(&self) -> &[u8];
    pub const fn is_finished(&self) -> bool;
    pub fn failure(&self) -> Option<&Error>;
}
impl Default for Utf8Stream { fn default() -> Self; }

#[derive(Clone, Copy, Debug)]
pub struct Utf8Proof<'a> {
    text: &'a str,
    summary: Utf8Summary,
}
impl<'a> Utf8Proof<'a> {
    pub const fn as_str(&self) -> &'a str;
    pub fn as_bytes(&self) -> &'a [u8];
    pub const fn summary(&self) -> Utf8Summary;
}

pub(crate) fn checked_len(len: usize) -> Result<u32>;
```

All `Utf8Stream` and `Utf8Proof` fields are private, accessible to descendant
wave modules through Rust's ancestor privacy rules. Do not add public unchecked
constructors, mutable byte accessors, or public proof issuance traits. There is
no new public error enum or trait. The canonical schema `ValidationProof` is
unrelated and is never issued by this crate.

The default summary is `(byte_len=0, scalar_len=0, ascii_only=true)`. Stream new,
default and reset produce empty pending storage, zero counters and quote carry,
ASCII-only true, finished false, and no failure. `bytes_seen` includes pending
bytes from successful chunks. `scalars_seen` counts only completed scalars.
`pending_bytes` exposes only the live incomplete prefix. The root helper rejects
lengths above `u32::MAX` with `PayloadTooLarge`, source Utf8, offset `u32::MAX`.

## Wave 1: validate.rs

```rust
pub fn validate(bytes: &[u8]) -> Result<Utf8Summary>;
pub fn validate_str(bytes: &[u8]) -> Result<&str>;
```

Both functions validate the complete slice and accept empty input. Reject
oversized input before converting positions to u32. Validate shortest UTF-8
encodings, the Unicode scalar range, and every continuation byte. Reject UTF-8
surrogates, overlong encodings, values above U+10FFFF, isolated continuation
bytes and incomplete terminal sequences. The first malformed sequence yields
`InvalidUtf8`, source Utf8, at its leading byte; a standalone invalid byte is
its own leading byte. A truncated sequence's offset is the start of that
sequence, not EOF. This matches `core::str::Utf8Error::valid_up_to()`.

Under `simd-utf8`, use the dependency's safe
`ps_simdutf8::compat::from_utf8(bytes)` facade, whose detailed error exposes
`valid_up_to` and `error_len`. Under other configurations use
`core::str::from_utf8(bytes)`. No direct ISA entry points or feature assumptions
are permitted. The neighboring dependency source provides this safe compat
API, including no-std fallback. Do not infer validity from a scanner mask.

`validate_str` borrows the exact input with its original lifetime. `validate`
counts Unicode scalars and actual bytes, and sets ASCII-only iff all bytes are
below 0x80. No normalization, BOM stripping, replacement characters, allocation,
or JSON string escape processing occurs.

## Wave 2: stream.rs

```rust
pub fn validate_chunk(state: &mut Utf8Stream, bytes: &[u8]) -> Result<()>;
pub fn finish(state: &mut Utf8Stream) -> Result<Utf8Summary>;
```

On a successful chunk, advance the checked absolute byte counter by the chunk
length exactly once. Complete any incoming pending prefix with up to four bytes
of bounded stack storage, then validate the remainder. Retain only a valid,
incomplete terminal prefix, at most three bytes. Count each scalar exactly once
when it becomes complete. Clear unused tail bytes and keep `utf8_tail_len` in
0..=3. `prev_in_string` and `prev_escaped` are untouched by stream operations.
An empty chunk is a no-op in a live state, including one with pending bytes.

Malformed bytes fail immediately, while a valid incomplete terminal prefix is
not an error until finish. Error offsets are absolute stream positions. A
failure involving a prefix from an earlier chunk reports that prefix's original
starting position. Checked cumulative byte overflow is `PayloadTooLarge` at
`u32::MAX`. Scalar counts cannot exceed the accepted byte count.

Chunk failure is transactional for counters, pending bytes and ASCII status:
leave their pre-call values intact, save the first Error in `failure`, and
return it. Further chunk and finish calls return a clone of that saved error
without consuming input. `finished` remains false in failed states.

`finish` on a live state with pending bytes saves and returns `InvalidUtf8` at
`total_bytes - utf8_tail_len`. Otherwise it marks finished and returns a complete
summary. Repeated successful finish is idempotent. Any chunk call after successful
finish returns `InvalidToken`, source Utf8, offset `total_bytes`, without changing
state, even for empty input. Reset explicitly begins a new stream. No standard
threading, hidden global state, or input retention beyond three pending bytes.

## Wave 3: proof.rs

```rust
pub fn validate(bytes: &[u8]) -> Result<Utf8Proof<'_>>;
#[cfg(feature = "alloc")]
pub fn validate_span<'a>(owner: &'a Payload, span: Span32) -> Result<Utf8Proof<'a>>;
pub fn slice<'a>(proof: &Utf8Proof<'a>, span: Span32) -> Result<&'a str>;
```

`validate` creates the private-field proof only after complete safe validation
and exact summary calculation. The stored str borrows the validated slice, not
a pointer reconstructed from an integer or an owner label. Copies of a proof
keep the same immutable borrow and cannot be retargeted.

`validate_span` checks owner length, checked span arithmetic, and slice bounds,
then validates that exact owner sub-slice. Its proof summary is relative to the
sub-slice, and UTF-8 error offsets are rebased to the owner with checked arithmetic.
Span overflow is `PayloadTooLarge` at span.offset. Out-of-bounds spans are
`UnexpectedEof` at span.offset. This API cannot yield decoded-arena proof or
schema proof; a separate borrow is needed for every independently owned buffer.

`slice` uses offsets relative to the proof bytes. Bounds and overflow have the
same errors as validate_span. Both ends must be Unicode scalar boundaries;
a non-boundary endpoint returns `InvalidUtf8` at the first offending endpoint.
Use safe str slicing. Empty slices are allowed at valid boundaries. A sub-slice
view does not validate any unrelated buffer merely because lengths match.

## Wave 4: fused.rs

```rust
pub fn validate_classified(
    bytes: &[u8],
    masks: &BlockMasks,
    state: &mut Utf8Stream,
) -> Result<()>;
```

This optional adapter feeds the same stream state with 0..=64 actual bytes and
may share scanner classification. It must first validate that valid_len equals
the slice length, valid_mask is exactly the low valid_len bits, and non_ascii_mask
has no high bits and exactly matches the actual byte high bits. No other mask
field or backend tag authorizes a memory read or UTF-8 proof. Public BlockMasks
can be forged, so an asserted all-ASCII mask is never sufficient by itself.

Lengths above 64 or metadata mismatch are `InvalidToken`, source Utf8, at the
current stream byte offset, leaving state fully unchanged. Oversized ABI totals
and terminal states follow stream rules once the metadata is accepted. After
metadata validation the results, counters, pending bytes, error precedence and
poisoning behavior are exactly `stream::validate_chunk`. In particular an ASCII
chunk cannot skip validation of an incoming non-ASCII pending prefix.
No bytes are loaded outside the provided slice and no optional dependency is
required by this adapter. A completely validated stream summary alone does not
create a borrowed proof for detached chunks.

## Cross-module invariants

- Input is immutable and lengths/offsets never truncate or wrap.
- All validation errors use the canonical Error and ErrorSource::Utf8. The
  error's schema and field IDs retain Error::new defaults.
- Stream equivalence holds across every split, including splits inside four-byte
  sequences and empty chunks. Finish is required for complete-stream validity.
- Count scalars, not UTF-16 code units or graphemes. A valid non-BMP character
  contributes four bytes and one scalar.
- No public ScanCarry setter exists on Utf8Stream; caller-forged tail lengths
  cannot enter the private state. Pending bytes are a valid incomplete prefix.
- Proofs are safe owner-bound str borrows; summary structs and raw masks alone
  never constitute evidence of validity.
- Dependency direction: proof -> validate; stream -> validate or safe core
  validation; fused -> stream. Shared state/types live at the root. Modules
  can import `super::*` without importing a sibling's private types.

The root declares the four separately owned wave modules. Wave 0 writes none
of those files and makes no claim of build or execution validation.
