# jmax-string: frozen Wave 0 contract

Status: frozen for Waves 1 through 4. Allowed dependencies are jmax-abi,
jmax-dispatch, jmax-buffer and jmax-utf8. No new dependency, allocation, or manifest
change is required. The root defines shared data independently of wave modules.
All exported functions are safe and available on every target.

## Canonical ABI and exact root definitions

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::traits::ByteSink;
pub const QUOTE: u8 = b'"';
pub const BACKSLASH: u8 = b'\';
pub const MAX_ESCAPE_BYTES: usize = 12;
pub const MAX_UTF8_SCALAR_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringScan {
    pub raw: Span32,
    pub content: Span32,
    pub next_offset: u32,
    pub first_escape: Option<u32>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StringLengths {
    pub byte_len: u32,
    pub scalar_len: u32,
}
impl StringLengths {
    pub const fn new(byte_len: u32, scalar_len: u32) -> Self;
    pub fn checked_add(self, other: Self) -> Result<Self>;
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedEscape {
    pub scalar: char,
    pub consumed: u8,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscapePolicy {
    pub escape_solidus: bool,
    pub ascii_only: bool,
}
pub(crate) fn checked_input_len(input: &[u8]) -> Result<u32>;
pub(crate) fn content_bytes(input: &[u8], content: Span32) -> Result<&[u8]>;
```

All result-struct fields are public. No result struct is validation evidence:
callers can construct arbitrary values, so any later safe API consuming spans
must check them again. No new public trait or error enum exists. Canonical
StrRef, SpanSource and ByteSink must not be shadowed by local equivalents.

StringLengths defaults to zero counts. Its checked_add returns
DecodedArenaOverflow, source Decode, offset zero on either u32 overflow, without
wrapping or partial results. EscapePolicy defaults to both fields false.
checked_input_len rejects lengths above u32::MAX with PayloadTooLarge, source
Decode, offset u32::MAX. content_bytes uses that check, returns PayloadTooLarge
at content.offset for overflowing span arithmetic, and UnexpectedEof at
content.offset for an out-of-bounds span; both use source Decode. Other modules
may perform their own size checks to preserve their specified error source.

## Wave 1: scan.rs

```rust
pub fn scan_string(input: &[u8], opening_quote: u32) -> Result<StringScan>;
pub fn first_special(bytes: &[u8]) -> Option<usize>;
```

scan_string requires a complete input slice, not a streaming fragment. Reject
input length above u32::MAX with PayloadTooLarge, source Scanner, offset
u32::MAX. An opening_quote outside input returns UnexpectedEof at opening_quote;
a byte other than a double quote returns InvalidToken there. Scan until the
first quote preceded by an even consecutive backslash count. All offsets in
the result are absolute input offsets. raw includes both quotes; content starts
one byte after the opening quote and ends before the closing quote; next_offset
is raw.end(). Empty content is allowed. first_escape is the first backslash
inside content, or None. Do not count escaped quotes as delimiters.

Reject unescaped raw bytes below 0x20 with ControlCharInString, source Scanner,
at the offending byte. Absence of a closing delimiter returns UnclosedString,
source Scanner, at opening_quote, including a final dangling backslash. Validate
raw content UTF-8 on a successfully bounded token, mapping UTF-8 errors to
absolute offsets with source Utf8. Scanning identifies escape boundaries but
need not validate escape spellings or surrogate pairs; success is not a decoded
string proof. Syntax/closure errors discovered during scanning precede this
post-scan UTF-8 validation. Strict users must invoke unescape or decoded_length
before accepting the string.

first_special returns the first index containing a quote, backslash, or byte
below 0x20, or None. It accepts arbitrary bytes and lengths, does not narrow to
u32, and does not validate UTF-8 or mutate carry. It is a discovery primitive.
Vector or SWAR discovery must never load beyond the supplied slice.

## Wave 2: unescape.rs

```rust
pub fn decode_escape(input: &[u8], backslash_offset: u32) -> Result<DecodedEscape>;
pub fn unescape(
    input: &[u8],
    content: Span32,
    sink: &mut dyn ByteSink,
) -> Result<StringLengths>;
```

decode_escape accepts an absolute offset into input and requires a backslash
there. Decode exactly one of quote, backslash, slash, b, f, n, r, t, or u plus
four hex digits. Simple escapes consume two source bytes. A nonsurrogate
Unicode escape consumes six. A high surrogate U+D800..U+DBFF requires an
immediately adjacent escaped low surrogate U+DC00..U+DFFF; combine them into
one scalar with 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00), consuming
twelve source bytes. Never encode a surrogate as UTF-8 or replace invalid input.
Accept upper/lowercase hex digits and embedded U+0000.

Error rules for decode_escape:

- Input length above u32::MAX: PayloadTooLarge at u32::MAX, source Decode.
- Offset outside input, or a truncated escape/code-unit spelling:
  UnexpectedEof at input.len(), source Decode.
- A non-backslash at the requested offset: InvalidEscape at that offset.
- Unknown escape letter or invalid hex digit: InvalidEscape at the offending
  byte. All such errors use source Decode.
- An isolated low surrogate: LoneSurrogate at its initial backslash.
- A complete high surrogate without an adjacent backslash-u prefix:
  LoneSurrogate at the high surrogate's initial backslash, including EOF.
- An adjacent low-surrogate escape with a truncated spelling: UnexpectedEof
  at EOF; invalid hex: InvalidEscape at the invalid digit. A complete second
  code unit outside the low-surrogate range: LoneSurrogate at the second
  escape's backslash. All surrogate errors use source Decode.

unescape accepts content without wrapper quotes and checks the span using
content_bytes. Decode escapes left to right and copy valid unescaped UTF-8
without normalization. Raw unescaped quotes are InvalidSyntax, source Decode;
raw controls are ControlCharInString, source Decode. Invalid raw UTF-8 yields
InvalidUtf8, source Utf8. All byte positions are absolute in input. Escape
lookahead is bounded by content.end(), even when more bytes follow the span:
bytes outside content never complete an escape or surrogate pair. Rebase the
bounded helper's EOF position to content.end() when appropriate.

Validate and measure the complete content before any sink call. Invalid input
or decoded byte/scalar u32 overflow therefore leaves the sink untouched.
Decoded count overflow is DecodedArenaOverflow, source Decode, at the source
position whose scalar would overflow. Reserve the exact decoded byte length,
then append UTF-8 to the sink. Never include wrapper quotes in output. The
returned lengths measure appended data only, excluding the sink's prior bytes.
An existing decoded-arena owner may use these lengths to form canonical StrRef
with its own checked arena offset; this crate cannot infer that owner.

Sink reserve/write errors propagate unchanged. After sink emission starts a sink
error may leave an emitted prefix; the API does not promise rollback. Arithmetic
must work on both 32-bit and 64-bit hosts. No unbounded temporary allocation.

## Wave 3: length.rs

```rust
pub fn decoded_length(input: &[u8], content: Span32) -> Result<StringLengths>;
pub fn utf8_length(text: &str) -> Result<StringLengths>;
```

decoded_length implements exactly the grammar, bounded-span, scalar, and error
rules of unescape, without retaining decoded output. It must return the same
counts and first error as unescape for an infallible sink. It may use a counting
ByteSink over unescape, or share the public decode_escape primitive and reproduce
strict UTF-8 segment validation. unescape must not call this module, keeping the
dependency graph acyclic and Wave 2 independent of Wave 3.

utf8_length measures an already valid str directly. Byte count is its actual
UTF-8 length; scalar count is chars().count(), not UTF-16 units or grapheme count.
Oversized text is PayloadTooLarge, source Decode, offset u32::MAX. In either
measurement a surrogate pair counts as one scalar and four decoded bytes.
An ASCII escape counts as one scalar with its actual decoded UTF-8 byte width.

## Wave 4: escape.rs

```rust
pub fn find_escape(bytes: &[u8], policy: EscapePolicy) -> Option<usize>;
pub fn escaped_length(text: &str, policy: EscapePolicy) -> Result<u32>;
pub fn write_escaped(
    text: &str,
    policy: EscapePolicy,
    sink: &mut dyn ByteSink,
) -> Result<()>;
pub fn write_string(
    text: &str,
    policy: EscapePolicy,
    sink: &mut dyn ByteSink,
) -> Result<()>;
```

find_escape accepts arbitrary bytes and finds the first quote, backslash,
control byte below 0x20, optionally slash, or optionally any non-ASCII byte,
according to policy. It returns a usize offset without narrowing. This is the
SIMD run-discovery primitive. Bulk-copy bytes before that position. SIMD paths
must check CPU/OS capability, cfg-gate architecture code, and use bounded loads;
reuse available safe dispatch when useful. Always retain a scalar fallback.
No unsupported target feature may be reached through a safe function.

write_escaped emits content only. write_string includes opening and closing
quotes. Mandatory escaping applies to quotes, backslashes and U+0000..U+001F.
Use the short spellings for quote, backslash, backspace, form-feed, newline,
carriage return and tab; use lowercase hex in other Unicode escapes. Escape
slash iff escape_solidus is true. Otherwise leave all permitted ASCII unchanged.
When ascii_only is false, emit valid non-ASCII UTF-8 unchanged. When true, emit
one backslash-u escape for a non-ASCII BMP scalar or an escaped UTF-16 surrogate
pair for a non-BMP scalar. Do not escape DEL, normalize text, or escape apostrophe.

escaped_length computes the exact content output byte count under these rules,
excluding wrapper quotes. Reject input above u32::MAX or output count above
u32::MAX with PayloadTooLarge, source Encode, offset u32::MAX. write_string
also checks that adding two quotes fits u32. Validate lengths and reserve the
entire append size before emitting any byte. Sink failures propagate unchanged;
write failures may leave a prefix. Input errors precede all sink calls.

## Cross-module invariants

- Scan spans are raw-payload byte ranges. Decoded byte lengths and scalar lengths
  are distinct counters; no escaped source length substitutes for either.
- No safe function trusts caller-constructed spans or StringScan as a proof.
  All narrowing, span addition and output-length arithmetic is checked.
- Controls and escapes cannot bypass validation through a SIMD run or a tail.
  Every physical load is bounded by the actual provided slice.
- Error positions refer to original input bytes. Serializer size failures use
  u32::MAX, sink errors preserve their original metadata, and synthetic
  StringLengths::checked_add overflow uses offset zero because it has no input.
- Default escaping followed by unescape reproduces the original UTF-8 exactly.
  ASCII-only output preserves scalars, including non-BMP characters and NUL.
- No schema ValidationProof is issued. Owner-bound UTF-8 views remain the
  responsibility of jmax-utf8 and the buffer owner.
- No circular type ownership: shared records live at the root; unescape can use
  UTF-8 validation, length can use unescape, and escape is independent of both.
  Descendant modules can import super::*.

The four module declarations intentionally precede separately owned wave files.
Wave 0 creates none of them and claims no build or execution validation.
