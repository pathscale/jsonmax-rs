# jmax-encode frozen contract, version 1

The normative root below freezes shared types, methods and constants. Modules import super::*; no ABI types or traits are redefined. Allowed dependencies are jmax-abi, jmax-buffer, jmax-string, jmax-number, jmax-decimal and jmax-dom. This crate is synchronous and no_std plus alloc. Use only public APIs of dependencies; the root exposes only stable ABI dependencies so implementations need no circular module types.

## Wave 1: sink.rs

```rust
pub fn reserve(sink: &mut dyn ByteSink, additional: usize, limit: usize) -> Result<()>;
pub fn append(sink: &mut dyn ByteSink, bytes: &[u8], limit: usize) -> Result<()>;
impl ByteSink for VecSink {
    fn reserve(&mut self, additional: usize) -> Result<()>;
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()>;
    fn len(&self) -> usize;
}
impl ByteSink for LimitedSink<'_> {
    fn reserve(&mut self, additional: usize) -> Result<()>;
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()>;
    fn len(&self) -> usize;
}
```

The free reserve/append limit is inclusive and relative to the supplied sink's len(). Compute len+additional with checked_output_len BEFORE delegating. reserve must not append bytes. append reserves then writes exactly the slice. reserve is a capacity assurance, not a size/accounting increment: a subsequent write must still check limits and must not double-charge the reservation. ByteSink's default write_byte/is_empty remain canonical.

VecSink::reserve checks its own total limit and uses Vec::try_reserve; write_bytes reserves then extends. A rejected write never changes its bytes. len is bytes.len(); into_vec transfers ownership without copying. A zero limit accepts an empty write only. No infallible capacity allocation or unchecked addition.

LimitedSink's limit applies only to bytes appended since new(), independent of the existing prefix. Its ByteSink::len returns appended_len(), not the total inner length. It checks its own limit before delegating. Successful writes must increase the inner length by exactly the requested byte count; reserve must leave length unchanged. Inner sinks are append-only while borrowed. An inner error may have appended a prefix: mark poisoned and return the original error. A violated length contract marks poisoned and yields InternalError. All future reserve/write calls on a poisoned sink fail with InternalError. Do not fabricate rollback for an arbitrary sink. VecSink is atomic per write, but a multi-write operation on any sink may have a visible prefix on failure. No implicit flush, blocking I/O or background work.

## Wave 2: writer.rs

```rust
impl<'a> JsonWriter<'a> {
    pub fn new(sink: &'a mut dyn ByteSink, limits: EncodeLimits) -> Self;
    pub fn begin_object(&mut self) -> Result<()>;
    pub fn end_object(&mut self) -> Result<()>;
    pub fn begin_array(&mut self) -> Result<()>;
    pub fn end_array(&mut self) -> Result<()>;
    pub fn key(&mut self, key: &str) -> Result<()>;
    pub fn null(&mut self) -> Result<()>;
    pub fn boolean(&mut self, value: bool) -> Result<()>;
    pub fn string(&mut self, value: &str) -> Result<()>;
    pub fn u64(&mut self, value: u64) -> Result<()>;
    pub fn i64(&mut self, value: i64) -> Result<()>;
    pub fn f64(&mut self, value: f64) -> Result<()>;
    pub fn decimal(&mut self, value: Decimal128) -> Result<()>;
    pub fn raw(&mut self, value: RawFragment<'_>) -> Result<()>;
    pub fn finish(self) -> Result<usize>;
}
```

new wraps the sink in LimitedSink and initializes an empty frame stack, root_written=false, poisoned=false. It performs no writes. Only one root is allowed. Root scalar depth=0; root container depth=1. max_depth is inclusive, so zero permits scalars only. All frames use fallible allocation. begin_* starts a value, appends the opening delimiter, updates the parent once and pushes a frame. end_* checks the matching frame and any required object value, then appends the close. Object frames alternate key/value and use count as property count; Array count is item count. Commas appear between members/items only; key emits escaped key plus colon. No whitespace, trailing commas or second root. Empty containers are valid. Duplicate key strings are permitted at the syntax writer layer, which keeps no global key map.

A value in an object requires a preceding key; a key requires an object expecting_key=true. Count a property when its key is accepted and an array item when its value starts. Do not count a child again on close. Reject count beyond max_container_items before writing that member/item. max_string_bytes bounds decoded UTF-8 bytes, including keys. max_number_bytes bounds emitted unquoted number token length; decimal's quoted contents are additionally subject to max_string_bytes. max_output_bytes bounds newly appended bytes including every quote, separator and escape, not any preexisting sink prefix. Use checked arithmetic for prefix+token sizing.

String escaping is exact: quote/backslash -> backslash escape; backspace/form-feed/newline/carriage-return/tab -> b/f/n/r/t escapes; other U+0000..U+001F -> lowercase `\\u00xx`; other valid UTF-8 bytes remain unchanged, including slash and non-ASCII. escaped_string_len includes both quotes and must agree with actual emission. Escape complete UTF-8 strings, not arbitrary byte sequences. Numeric integers are exact base 10 with no plus, padding or leading zeros. f64 rejects NaN/infinity with InvalidNumber and emits finite shortest-roundtrip JSON numeric text; negative zero is emitted `-0`. decimal emits a JSON STRING with exact fixed scale, no exponent, and a decimal point only if scale>0. Scale <=38; invalid scale -> DecimalScaleMismatch. Handle i128::MIN without signed negation overflow and use leading fractional zeros as needed. Decimal128{coefficient:123,scale:2} emits `"1.23"`.

raw accepts only a RawFragment, and revalidates it under this writer's limits before emission because it may have been constructed with looser limits. Check current open-container depth + fragment.depth <= max_depth. Its complete bytes are one value for the surrounding syntax state; internal keys/items are not parent members. Revalidation enforces nested counts and string/number bounds, not merely total byte size/depth. Raw spelling and whitespace are preserved exactly.

Any writer operation returning an error poisons the writer. No later operation may continue or finish successfully. Validate syntax/state/limits and reserve complete tokens before modifying frame state where possible; an arbitrary ByteSink can fail partway through multi-write output, so callers must discard the partial message. finish consumes the writer and requires a root, an empty frame stack and no poisoning; it returns newly appended length. No root/unclosed container -> UnexpectedEof, bad operation order/mismatched close/second root -> InvalidSyntax, poisoned use -> InternalError. Dropping an unfinished writer never closes containers automatically or claims success.

## Wave 3: raw.rs

```rust
pub fn validate(bytes: &[u8], limits: EncodeLimits) -> Result<RawFragment<'_>>;
pub fn emit(writer: &mut JsonWriter<'_>, fragment: RawFragment<'_>) -> Result<()>;
```

validate is the sole public RawFragment constructor. It borrows an immutable slice and strictly validates one entire JSON value, permitting only JSON whitespace (space, TAB, LF, CR) before/after it. Return the exact original bytes including that whitespace. Reject empty/all-whitespace input, a BOM, multiple values, trailing nonwhitespace, and incomplete containers/tokens. Validate UTF-8, unescaped controls, legal escapes and correct UTF-16 surrogate pairing. Validate the complete JSON number grammar; no NaN/infinity/leading plus/leading zeros or missing digits. A syntactically valid large numeric token need not fit a machine number here. Object keys must be strings, separators/colons must be exact, and trailing commas fail. Duplicate object keys are syntactically legal.

Use bounded iterative container state, never unbounded host-stack recursion. Depth/count/string/number/output limits are inclusive and match writer semantics. Count decoded string UTF-8 bytes, including the UTF-8 encoding size of escaped codepoints/surrogate pairs; scanning escape spelling length is insufficient. Strings/keys must not exceed max_string_bytes, numbers must not exceed max_number_bytes, each container must respect max_container_items, and the entire input must fit max_output_bytes. Store the maximum simultaneously open containers in RawFragment.depth (scalar is zero). kind records the root token category excluding whitespace. Construction occurs only after successful EOF. A borrowed fragment cannot outlive or safely mutate its underlying bytes.

emit delegates exactly to writer.raw, preserving its syntax state and failure behavior. No public unchecked constructor, From<&[u8]>, transmute, proof-only shortcut or direct UnvalidatedSpan emission. ABI document/slot fields are public and may have been mutated, so a copied proof cannot replace validation of exact fragment bytes. Allowed dependencies do not include jmax-syntax; implement the bounded strict grammar here or use only existing allowed string/number APIs with fully checked framing.

Errors use the most precise ABI codes: UnexpectedEof, InvalidSyntax, TrailingContent, InvalidUtf8, InvalidEscape, LoneSurrogate, ControlCharInString or InvalidNumber. Invalid spans are never constructed. Depth -> DepthLimitExceeded; whole-byte overflow/limit -> PayloadTooLarge; string/count/number resource bounds and allocation failure -> BudgetExhausted. Source is Encode; raw parser byte_offset is relative to supplied fragment.

## Wave 4: typed.rs

```rust
pub fn encode<T: JsonEncode + ?Sized>(value: &T, sink: &mut dyn ByteSink, limits: EncodeLimits) -> Result<()>;
pub fn encode_to_vec<T: JsonEncode + ?Sized>(value: &T, limits: EncodeLimits) -> Result<Vec<u8>>;
pub fn encode_slots(slots: &ValidatedSlots, schema: &SchemaIr, sink: &mut dyn ByteSink, limits: EncodeLimits) -> Result<()>;
```

encode invokes the canonical ABI JsonEncode trait against a bounded temporary VecSink, then raw::validate on the ENTIRE produced buffer under limits, then appends it to the caller's sink under a new-output byte limit. This prevents a safe custom implementation from publishing malformed/multiple-root JSON. User code in JsonEncode remains responsible for its own computation/termination. encode_to_vec returns that same validated owned buffer without an extra copy. Trait, validation and size failures occur before any caller-sink write. A caller sink failure may leave a prefix. Do not add orphan implementations of ABI traits for foreign primitive types.

encode_slots emits one object in schema.fields order using JsonWriter. ABI field index i maps to slots.slots[i]; require exact vector length. Validate schema before emission: globally unique names and FieldIds, exact required_count, valid scalar-only constraint families, and ordered min/max bounds. Field scalar_kind None is unconstrained; Number with an integer bound requires integer grammar; other Number accepts JSON number; True/False both denote Boolean category; String/Null require that category. Length bounds count decoded Unicode scalars. Names may be empty. Empty means absent; omit optional fields and reject a missing required field with MissingRequiredField. No output field outside the schema is emitted, so reject_unknown introduces no extra output keys.

Convert Bool/U64/I64/F64Bits/Str/Decimal/RawSpan to their writer forms. Decimal emits fixed-point quoted text, consistent with writer.decimal. Resolve Str against exact payload/decoded owners with checked span, UTF-8 and scalar_len. Revalidate each RawSpan using raw::validate and apply the field's scalar/length/integer constraints to its actual parsed value. Do not assume RawSpan is unvalidated-free merely because slots contains a proof. Null is represented as RawSpan of `null`; Empty must never emit null. Reject type mismatches, out-of-range integers and nonfinite floats. For min/max comparisons, do not narrow large U64 or round floating/decimal values through f64. String bounds apply to decoded output contents, including a Decimal's resulting quoted text when the schema expects String.

The ABI SchemaIr has no decimal/pattern graph metadata, and this crate is not allowed to depend on schema-compile, schema-vm or format. Version 1 encode_slots therefore rejects ANY field.format=Some(...) with UnsupportedSchemaFeature during schema preflight, including empty/unknown names. It never silently claims named-format validation. Graph-aware callers validate their richer schema separately; do not introduce a hidden forbidden dependency. A slots proof is not a schema-fingerprint match check here: all supported output constraints are actually rechecked against supplied schema and current owners. Invalid ranges/malformed schema -> ConstraintViolation; scalar mismatch -> TypeMismatch; string/number bounds -> ConstraintViolation; missing required -> MissingRequiredField. Attach FieldId/schema_id where known.

Build and validate the COMPLETE result in a bounded temporary sink, finish its writer, then append to the external sink. Thus schema/value errors do not partially emit an object externally. Empty fields schema emits `{}`. Arbitrary malformed slot contents cannot panic or bypass raw/string checks.

## Cross-module invariants

All originating errors use ErrorSource::Encode and exact raw offsets where available; generic output/sink errors use zero. Propagate external sink errors faithfully. PayloadTooLarge covers whole-output size/arithmetic limits, BudgetExhausted covers allocation/container/string/number limits. The roots' checked_output_len and escaped_string_len are authoritative shared helpers. Limits are per operation/message and never global mutable state. ByteSink reserve never promises eventual validation or commits a logical length. The writer creates syntax, raw validates borrowed complete values, and typed validates a complete buffered message before publishing it. No component issues or fabricates ValidationProof.

## Normative shared root

```rust
//! Bounded JSON output with syntax state and explicit validated-fragment ownership.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::Decimal128;
pub use jmax_abi::schema::{SchemaFieldIr, SchemaIr, SlotValue, ValidatedSlots};
pub use jmax_abi::traits::{ByteSink, JsonEncode};

pub mod sink;
pub mod writer;
pub mod raw;
pub mod typed;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1_024 * 1_024;
pub const DEFAULT_MAX_DEPTH: u16 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeLimits {
    pub max_output_bytes: usize,
    pub max_depth: u16,
    pub max_container_items: u32,
    pub max_string_bytes: u32,
    pub max_number_bytes: u32,
}

impl Default for EncodeLimits {
    fn default() -> Self {
        Self { max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES, max_depth: DEFAULT_MAX_DEPTH,
            max_container_items: 1_000_000, max_string_bytes: 4 * 1_024 * 1_024,
            max_number_bytes: 1_024 }
    }
}

#[derive(Clone, Debug)]
pub struct VecSink { pub(crate) bytes: Vec<u8>, pub(crate) limit: usize }

impl VecSink {
    pub const fn new(limit: usize) -> Self { Self { bytes: Vec::new(), limit } }
    pub fn as_slice(&self) -> &[u8] { &self.bytes }
    pub fn into_vec(self) -> Vec<u8> { self.bytes }
    pub const fn limit(&self) -> usize { self.limit }
}

/// Limits newly appended bytes independently of existing bytes in the inner sink.
pub struct LimitedSink<'a> {
    pub(crate) inner: &'a mut dyn ByteSink,
    pub(crate) start_len: usize,
    pub(crate) limit: usize,
    pub(crate) poisoned: bool,
}

impl<'a> LimitedSink<'a> {
    pub fn new(inner: &'a mut dyn ByteSink, limit: usize) -> Self {
        let start_len = inner.len();
        Self { inner, start_len, limit, poisoned: false }
    }
    pub fn appended_len(&self) -> usize { self.inner.len().saturating_sub(self.start_len) }
    pub const fn is_poisoned(&self) -> bool { self.poisoned }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteContainer { Object, Array }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteFrame {
    pub kind: WriteContainer,
    pub count: u32,
    pub expecting_key: bool,
}

pub struct JsonWriter<'a> {
    pub(crate) sink: LimitedSink<'a>,
    pub(crate) limits: EncodeLimits,
    pub(crate) frames: Vec<WriteFrame>,
    pub(crate) root_written: bool,
    pub(crate) poisoned: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragmentKind { Null, Boolean, Number, String, Object, Array }

/// The only public constructor is the strict parser in `raw::validate`.
#[derive(Clone, Copy, Debug)]
pub struct RawFragment<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) kind: FragmentKind,
    pub(crate) depth: u16,
}

impl<'a> RawFragment<'a> {
    pub fn as_bytes(self) -> &'a [u8] { self.bytes }
    pub const fn kind(self) -> FragmentKind { self.kind }
    /// Maximum simultaneously open containers, relative to this fragment's root.
    pub const fn depth(self) -> u16 { self.depth }
}

/// Exact quoted JSON length for the canonical minimal escaping used by the writer.
pub fn escaped_string_len(text: &str) -> Result<usize> {
    let mut size = 2usize;
    for &byte in text.as_bytes() {
        let width = match byte {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 2,
            0..=0x1f => 6,
            _ => 1,
        };
        size = size.checked_add(width).ok_or_else(||
            encode_error(ErrorCode::PayloadTooLarge, "escaped string size overflow"))?;
    }
    Ok(size)
}

pub fn checked_output_len(current: usize, additional: usize, limit: usize) -> Result<usize> {
    let next = current.checked_add(additional).ok_or_else(||
        encode_error(ErrorCode::PayloadTooLarge, "output size overflow"))?;
    if next > limit {
        return Err(encode_error(ErrorCode::PayloadTooLarge, "output size limit exceeded"));
    }
    Ok(next)
}

pub(crate) fn encode_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::Encode, 0, detail)
}
```
