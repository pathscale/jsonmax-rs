# jmax-scan-neon: frozen Wave 0 contract

Owner: W0-ROOTS-1. Status: frozen for Waves 1 through 4. The root owns shared definitions; later
waves implement only their assigned modules. All safe functions work on every
target and never execute unsupported instructions. No allocation is required.
The only dependency used by this crate is `jmax-abi`.

## Canonical ABI and root API

The root declares `pub mod classify; pub mod quotes; pub mod structural; pub mod tails;`.
All four children may `use super::*`. The root also provides a forwarding
`pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks>`
which calls classify::classify with unchanged arguments and result.

The root publicly reexports these exact canonical ABI types, without wrappers:

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};
pub const BLOCK_BYTES: usize = 64;
pub const LANE_BYTES: usize = 16;
pub const VECTOR_LANES: usize = 4;
pub const BACKEND: BackendId = BackendId::Neon;
pub const BACKEND_TAG: u8 = 1;
pub fn is_supported() -> bool;
pub fn valid_mask(len: usize) -> Result<u64>;
```

`ClassifyFn` is exactly `fn(&[u8], &mut ScanCarry) -> Result<BlockMasks>`.
`BlockMasks` and `ScanCarry` retain every field and layout in the frozen ABI.
Tags are scalar 0, NEON 1, AVX2 2, AVX-512 3, independently of Rust enum layout.
`is_supported` uses std::arch::is_aarch64_feature_detected!("neon") on aarch64
with std. Without std on aarch64 it accepts only cfg!(target_feature = "neon").
All other targets, including arm32, return false and use the local scalar
fallback. Native functions are cfg-gated to aarch64. The root forbids unsafe
operations in an unsafe function unless they have an explicit unsafe block. `valid_mask` returns zero for zero, all ones for 64, and the low
`len` bits otherwise; lengths above 64 return `InvalidToken` at offset zero.

These root-owned definitions are exact. All listed fields are public.

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructuralMasks {
    pub colon_mask: u64,
    pub comma_mask: u64,
    pub lbrace_mask: u64,
    pub rbrace_mask: u64,
    pub lbracket_mask: u64,
    pub rbracket_mask: u64,
    pub whitespace_mask: u64,
    pub control_mask: u64,
    pub digit_mask: u64,
    pub non_ascii_mask: u64,
}
impl StructuralMasks {
    pub const fn punctuation_mask(&self) -> u64;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawMasks {
    pub quote_mask: u64,
    pub backslash_mask: u64,
    pub classes: StructuralMasks,
    pub backend: BackendId,
}
impl Default for RawMasks { fn default() -> Self; }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuoteMasks {
    pub escaped_quote_mask: u64,
    pub delimiter_quote_mask: u64,
    pub in_string_mask: u64,
}
```

Defaults are zero masks and `BackendId::Scalar`. `punctuation_mask` is the OR of
the six punctuation masks. No crate-specific public trait or error enum exists.
The root also provides these crate-private integration helpers, fully implemented:

```rust
pub(crate) fn validate_carry(carry: &ScanCarry) -> Result<()>;
impl RawMasks {
    pub(crate) fn finish(self, quotes: QuoteMasks, len: usize) -> Result<BlockMasks>;
}
```

`validate_carry` requires `prev_in_string` and `prev_escaped` to be 0 or 1,
returning `InternalError` otherwise. It does not inspect UTF-8 state. `finish`
checks the length, clips every mask, sets the backend tag and valid length,
and computes structural punctuation outside string content. It does not mutate
carry or calculate quotes. Callers must pass quote results for the same block.

## Wave 1: classify.rs

```rust
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks>;
pub fn classify_block(input: &[u8; BLOCK_BYTES], carry: &mut ScanCarry) -> Result<BlockMasks>;
pub fn classify_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks;
```

`classify` is the canonical `ClassifyFn` entry point. It accepts exactly 0 through
64 bytes, not an arbitrary-length stream. It validates length and quote carry,
obtains bounded storage with `tails::load_block`, computes raw classes, applies
`quotes::quote_masks` using the original length, then calls `RawMasks::finish`.
It may use the full-block fast path when the input length is 64. `classify_block`
is the equivalent fixed-array pipeline with valid length 64. Neither publishes
carry changes until every fallible operation has succeeded. Empty input yields
zero masks, zero valid length, and unchanged carry. Its backend tag is Scalar.

`classify_raw` returns all full-block raw masks without inspecting or changing
carry. It combines quote/backslash classification with
`structural::classify_structural`. On supported aarch64 use four real 16-byte
NEON lanes; lane k contributes positions 16*k through 16*k+15. Compare bytes as
unsigned values and reduce each comparison vector to its 16-bit byte-position
mask without relying on host endianness. Widen each lane mask to u64 before
shifting by 16*k. A NEON-tagged result requires actual NEON classification;
a scalar-only implementation with a native tag violates this contract.

Unsupported CPU/OS/build configurations use `tails::classify_scalar_raw`, and
the returned backend describes the path actually executed. SIMD functions with
architecture types or target-feature attributes stay private and cfg-gated.
Every safe entry point checks is_supported before native execution and never
assumes the dispatcher has already checked CPU support. Raw and structural
classification must choose the same native versus scalar path.

## Wave 2: quotes.rs

```rust
pub fn quote_masks(
    quote_mask: u64,
    backslash_mask: u64,
    valid_len: u8,
    carry: &mut ScanCarry,
) -> Result<QuoteMasks>;
```

Ignore input mask bits above `valid_len`. A quote is escaped exactly when it
follows an odd-length consecutive backslash run, including an incoming run.
Track this byte property even outside a string; grammar validation belongs to
later layers. A non-backslash resets run parity after its escape status has
been determined. Every unescaped quote toggles the string state. The opening
and closing delimiter bits are absent from `in_string_mask`; all other bytes
visited while inside a string are present, including backslashes and escaped
quotes. `escaped_quote_mask` contains raw quote bits with odd preceding parity;
`delimiter_quote_mask` contains all remaining raw quote bits.

`prev_in_string` is the outgoing state as 0 or 1. `prev_escaped` is the parity
of the trailing backslash run as 0 or 1. On an empty block preserve both fields.
Always preserve `utf8_tail` and `utf8_tail_len` byte for byte. SIMD or bitwise
prefix operations must match these semantics across all four 16-byte lane boundaries and block edges. Quote resolution
consumes the combined 64-bit masks, not independent lane states; it may use
portable bit operations or scalar parity logic without additional ISA features.
There is one carry transition per actual byte, never per padding byte.

Errors: length above 64 is `InvalidToken`; noncanonical quote carry or overlapping
quote/backslash bits at live positions is `InternalError`. All use
`ErrorSource::Scanner`, offset zero, and leave carry unchanged. Unclosed strings
and trailing escapes are legitimate carry states, not errors in this layer.

## Wave 3: structural.rs

```rust
pub fn classify_structural(input: &[u8; BLOCK_BYTES]) -> StructuralMasks;
```

Return full-block raw masks: each punctuation mask matches its ASCII spelling;
whitespace matches only space, tab, CR, LF; control matches every byte below
0x20; digit matches ASCII 0 through 9; non-ASCII matches every byte at least
0x80. Whitespace and control therefore overlap for tab, CR, LF. No class is
filtered by string state. No Unicode whitespace or non-ASCII digits are included.
Use four 16-byte NEON vectors with unsigned comparisons and the same byte-mask
reduction order as classify_raw. The native path must actually classify the
bytes with NEON, not return a scalar result carrying a NEON tag. This safe
function independently selects a complete local scalar implementation when
is_supported is false. It performs no allocation, UTF-8 validation, JSON grammar
validation, or carry update and has no error path.

## Wave 4: tails.rs

```rust
pub fn load_block(input: &[u8]) -> Result<[u8; BLOCK_BYTES]>;
pub fn classify_scalar_raw(input: &[u8; BLOCK_BYTES]) -> RawMasks;
```

`load_block` rejects lengths above 64 with `InvalidToken`, source Scanner,
offset zero. Otherwise copy exactly the available bytes into a zero-initialized
64-byte array. No read beyond the slice, alignment requirement, guard-page
assumption, or caller-provided padding is permitted. Full-block loads read only
an actual 64-byte object. Four unaligned 16-byte NEON loads are allowed only
from the complete 64-byte array, never directly from an undersized input slice.
On a partial input the zeroed copy supplies safe native load storage, but the
true input length still controls every output mask and quote-carry transition.

`classify_scalar_raw` implements the complete raw classification rules locally,
including quotes and backslashes, and always sets backend Scalar. It must be
available on non-aarch64 and no_std targets, including arm32. It must not call classify, quotes, or
structural, keeping the fallback dependency graph acyclic. `load_block` likewise
does not call those modules.

## Cross-module invariants and errors

- Bit `i` means input byte `i`, in increasing address order, independently of
  host endianness. Every observable ABI mask is ANDed with `valid_mask`.
- `quote_mask` is raw, including escaped quotes. Individual punctuation,
  whitespace, control, digit, non-ASCII and backslash masks are also raw.
  Only `structural_mask` is filtered by `!in_string_mask`.
- `structural_mask` is exactly the OR of the six punctuation masks intersected
  with `!in_string_mask & valid_mask`. Quotes are never structural punctuation.
- Padded zero bytes do not contribute control bits or carry. Quote analysis
  uses the true length, never 64 for a partial block.
- Splitting bytes at arbitrary block boundaries produces the same quote state
  and byte masks as continuous processing. No scanner function finalizes UTF-8
  or reports a string error merely because a block ends in a string.
- Scanner error positions are block-relative. The stream owner adds its checked
  base offset. No input length or pointer is narrowed before validating bounds.
- Length and caller-state errors have no side effects. The scanner does not
  reject JSON syntax, invalid escapes, control bytes, or UTF-8 at this layer;
  it reports their raw masks for consumers to validate.
- Module call graph: classify -> tails/raw fallback, structural, quotes; quotes
  -> root helpers; structural -> private implementation; tails -> root types.
  Modules import `super::*`; no shared definition is owned by another wave.

The root intentionally declares the four wave modules before their separately
owned files are delivered. Wave 0 does not authorize creating those files or
changing manifests. No build or execution validation is claimed.
