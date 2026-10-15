# jmax-error frozen contract

Owner: W0-ROOTS-1. The sole crate dependency is jmax-abi. Use core and alloc, and
support no_std with an allocator. Error, ErrorCode, ErrorSource, and Result are
canonical reexports, never wrapper replacements.

## Root vocabulary

The root declares `pub mod codes; pub mod paths; pub mod diagnostic; pub mod redaction;`.
Children may `use super::*`; String and Vec are root-private alloc imports.

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::span::Span32;
pub const DEFAULT_MAX_PATH_SEGMENTS: u16 = 64;
pub const DEFAULT_MAX_PATH_BYTES: u32 = 4096;
pub const DEFAULT_MAX_DISPLAY_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    Syntax, Encoding, Number, Limit, Schema, Validation, Framing, Transport,
    Authentication, Cancelled, Internal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathKind { Json, Schema }
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PathSegment { Key(String), Index(u32) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathLimits { pub max_segments: u16, pub max_encoded_bytes: u32 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedPath {
    pub(crate) kind: PathKind,
    pub(crate) limits: PathLimits,
    pub(crate) segments: Vec<PathSegment>,
    pub(crate) encoded_len: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceLocation { pub byte_offset: u32, pub line: u64, pub column: u64 }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextLimits { pub before: u16, pub after: u16 }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ByteContext {
    pub(crate) bytes: Vec<u8>,
    pub(crate) span: Span32,
    pub(crate) focus: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub error: Error,
    pub json_path: Option<BoundedPath>,
    pub schema_path: Option<BoundedPath>,
    pub location: Option<SourceLocation>,
    pub context: Option<ByteContext>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedactionPolicy {
    pub include_detail: bool,
    pub include_paths: bool,
    pub include_context: bool,
    pub max_output_bytes: usize,
}
#[derive(Clone, Copy)]
pub struct SafeDisplay<'a> {
    pub(crate) diagnostic: &'a Diagnostic,
    pub(crate) policy: RedactionPolicy,
}
#[derive(Clone, Copy)]
pub struct SafeError<'a> {
    pub(crate) diagnostic: &'a Diagnostic,
    pub(crate) policy: RedactionPolicy,
}
```

The root implements exactly these methods and traits:

```rust
impl Default for PathLimits { fn default() -> Self; }
impl BoundedPath {
    pub const fn kind(&self) -> PathKind;
    pub const fn limits(&self) -> PathLimits;
    pub fn segments(&self) -> &[PathSegment];
    pub const fn encoded_len(&self) -> u32;
    pub fn is_empty(&self) -> bool;
}
impl Default for ContextLimits { fn default() -> Self; }
impl ByteContext {
    pub fn bytes(&self) -> &[u8];
    pub const fn span(&self) -> Span32;
    pub const fn focus(&self) -> u32;
}
impl Diagnostic { pub const fn new(error: Error) -> Self; }
impl RedactionPolicy { pub const fn safe_default() -> Self; }
impl Default for RedactionPolicy { fn default() -> Self; }
```

PathLimits defaults to the two path constants. ContextLimits defaults to 24
bytes before and 24 bytes after the offset. Diagnostic::new leaves every optional
field None. RedactionPolicy defaults to safe_default, which sets all include
flags false and max_output_bytes to DEFAULT_MAX_DISPLAY_BYTES. Root-private
`path_error(kind: PathKind, code: ErrorCode, detail: &'static str) -> Error` uses
source Syntax for Json or SchemaCompile for Schema and byte_offset 0.

## Wave 1: src/codes.rs

```rust
pub const fn category(code: ErrorCode) -> ErrorCategory;
pub const fn code_name(code: ErrorCode) -> &'static str;
pub const fn source_name(source: ErrorSource) -> &'static str;
pub const fn category_name(category: ErrorCategory) -> &'static str;
pub const fn is_incomplete(code: ErrorCode) -> bool;
pub const fn is_retryable(code: ErrorCode) -> bool;
```

Names are exact Rust variant identifiers, including capitalization. Match every
canonical variant explicitly; no Debug allocation and no wildcard catch-all.
The frozen category mapping is:

| Category | ErrorCode variants |
| --- | --- |
| Syntax | UnexpectedEof, InvalidSyntax, InvalidToken, TrailingContent, UnclosedString |
| Encoding | InvalidEscape, LoneSurrogate, InvalidUtf8, ControlCharInString |
| Number | InvalidNumber, NumberOverflow, DecimalPrecisionOverflow, DecimalScaleMismatch, TickMultipleViolation |
| Limit | DepthLimitExceeded, PayloadTooLarge, DecodedArenaOverflow, BudgetExhausted |
| Schema | UnsupportedSchemaFeature, SchemaImageInvalid |
| Validation | DuplicateObjectKey, UnknownObjectKey, MissingRequiredField, TypeMismatch, ConstraintViolation, PatternMismatch, FormatMismatch |
| Framing | FramingError |
| Transport | TransportIncomplete |
| Authentication | AuthFailed, SignatureInvalid |
| Cancelled | Cancelled |
| Internal | InternalError |

Is_incomplete is true only for UnexpectedEof and TransportIncomplete.
Is_retryable is true only for TransportIncomplete and means more transport bytes
may allow progress. It does not authorize replaying a request. These functions
cannot fail and do not inspect payloads or source-dependent context.

## Wave 2: src/paths.rs

```rust
impl BoundedPath {
    pub fn new(kind: PathKind, limits: PathLimits) -> Self;
    pub fn push_key(&mut self, key: &str) -> Result<()>;
    pub fn push_index(&mut self, index: u32) -> Result<()>;
    pub fn pop(&mut self) -> Option<PathSegment>;
    pub fn clear(&mut self);
    pub fn to_pointer(&self) -> Result<String>;
}
```

Paths use JSON Pointer text with no URI fragment encoding: root is the empty
string; every segment has one leading slash; key `~` becomes `~0`, `/` becomes
`~1`; other UTF-8 bytes are preserved. Index is base-10 u32 without leading zeros.
An empty key produces `/`. JSON and schema paths use the same encoding and
different kind tags. Key and index segments retain their semantic distinction
even if their pointer spelling is equal.

Encoded_len is exactly the encoded UTF-8 byte length, including slashes and
escapes. Before mutation, check segment count + 1 <= max_segments, encoded length
addition without overflow, and new encoded length <= max_encoded_bytes. Zero
limits allow the empty root. Allocate owned key bytes and vector space using
try_reserve before committing logical state. Limit/count/arithmetic violations
return BudgetExhausted through path_error with a static detail. Allocation
failures also return BudgetExhausted. Failure leaves kind, limits, segments and
encoded_len unchanged; spare capacity may change. Pop subtracts exactly the
removed segment's encoded length; clear resets length and segments and retains
allocation. To_pointer reserves fallibly and cannot exceed the frozen limit.
No truncation, silent omission, normalization, or unchecked integer casts.

## Wave 3: src/diagnostic.rs

```rust
pub fn locate(input: &[u8], byte_offset: u32) -> Result<SourceLocation>;
pub fn context(
    input: &[u8], byte_offset: u32, limits: ContextLimits,
) -> Result<ByteContext>;
pub fn build(
    error: Error, input: &[u8], limits: ContextLimits,
) -> Result<Diagnostic>;
```

Both locate and context require input.len <= u32::MAX and byte_offset <= len.
EOF is valid. Oversized input returns PayloadTooLarge/Cursor at u32::MAX;
out-of-range offset returns InvalidToken/Cursor at the requested offset. Locate
counts LF bytes strictly before byte_offset. Line is 1 + that count; column is
1 + bytes since the last LF, or byte_offset + 1 if none. A position on LF belongs
to the preceding line. CR is an ordinary byte. Invalid UTF-8 and multibyte text
are valid inputs, with columns measured in bytes. u64 counters represent the
EOF location even for u32::MAX newline bytes.

Context selects [offset.saturating_sub(before), min(len, offset + after)) with
nonwrapping arithmetic, computing the upper bound in usize or u64. The offset is
the boundary immediately before the current byte; after=1 includes that byte.
At EOF no byte at the offset exists. Return an owned copy of this bounded range,
Span32 of the original coordinates, and focus = offset - start. Focus may equal
bytes.len, including an empty context. No UTF-8 repair or lossy conversion.
Use try_reserve; failure returns BudgetExhausted/Cursor at byte_offset. Build
preserves the entire supplied Error, attaches Some(locate) and Some(context),
and leaves both paths None. Diagnostic construction never rewrites the original
error code, source, detail, schema_id or field_id. There are no references to
input in the result, so a tiny diagnostic cannot retain a large payload.

## Wave 4: src/redaction.rs

```rust
pub fn display(diagnostic: &Diagnostic, policy: RedactionPolicy) -> SafeDisplay<'_>;
pub fn error(diagnostic: &Diagnostic, policy: RedactionPolicy) -> SafeError<'_>;
pub fn redact(diagnostic: &Diagnostic, policy: RedactionPolicy) -> String;
impl core::fmt::Display for SafeDisplay<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}
impl core::fmt::Debug for SafeDisplay<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}
impl core::fmt::Display for SafeError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}
impl core::fmt::Debug for SafeError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}
impl core::error::Error for SafeError<'_> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)>;
}
```

All four format implementations and redact produce identical policy-controlled
text. SafeError::source always returns None; do not expose Diagnostic or Error
through a chained source or a derived Debug implementation. Formatter width,
precision and alternate mode must not expand output beyond the byte budget.

Untruncated format, in order, is `CodeName [SourceName] at byte N`; append
` (line L, column C)` when location is present. If either schema_id or field_id
is nonzero, append ` schema=S field=F`. Then conditionally append:

1. ` detail="TEXT"` if include_detail.
2. ` json_path="POINTER"` and ` schema_path="POINTER"` for present paths if
   include_paths. These labels follow the Diagnostic field, not the PathKind tag.
3. ` context@START="BYTES"` for present context if include_context.

Numbers use plain decimal. Code/source names come from codes.rs, never from the
underlying Error's Display. All quoted fields are escaped byte-by-byte into
ASCII: printable bytes 0x20..=0x7e other than quote and backslash pass through;
quote and backslash become `\"` and `\\`; all other bytes become `\xhh` with
lowercase hex. Thus newlines, terminal controls, bidi characters and invalid
UTF-8 cannot become active display characters. Pointer escaping occurs before
this display escaping. Default policy emits no detail, paths or context.

The output, including labels and optional marker, is at most max_output_bytes.
Zero budget emits empty text. Reserve up to min(3, budget) bytes for a final
truncation marker while streaming. If all content fits, omit the reservation
and marker. Otherwise emit the longest prefix that fits before the reservation
without splitting an escape sequence or UTF-8 codepoint, then that many dots.
There is no partial escape token and no unbounded intermediate formatting of
payload data. Use a two-pass count/emit strategy with saturating length counts
or a bounded buffer of at most the budget; do not allocate an entire escaped
path/context first. Redact is an infallible allocating convenience with normal
alloc allocation-failure behavior; Display adapters must be usable without
allocating output and propagate formatter errors.

## Cross-module invariants

Root-owned representations are never redeclared. Path and context copies are
bounded before allocation. All generated ABI error details are static and
contain no input bytes. Diagnostic and ByteContext's ordinary derived Debug are
trusted inspection APIs and may expose data; only SafeDisplay and SafeError
promise redaction. No Display/Error implementation is added to Diagnostic.
None of these operations validates JSON, UTF-8, schemas, or authorization.
No global formatting policy, environment access, logging or output side effects.

Dependency direction: codes and paths use root only; diagnostic uses root only;
redaction uses codes and the public path/context accessors. No reverse dependency
on redaction and no external error/logging crate.
