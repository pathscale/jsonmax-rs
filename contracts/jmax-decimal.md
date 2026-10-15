# jmax-decimal frozen Wave 0 contract

Owner: W0-ROOTS-3, row 11. Allowed dependency: `jmax-abi` only. The root
`crates/jmax-decimal/src/lib.rs` owns all shared definitions. Wave files use
`use super::*;`. They must not redefine ABI types or change the root. All four
modules are public. This crate uses `core` and the ABI, without allocation.

## Shared public surface

These are canonical re-exports, not new wrapper types:

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{Decimal128, DecimalPolicy};
pub use jmax_abi::span::Span32;
pub use jmax_abi::traits::ByteSink;

pub const MAX_PRECISION: u8 = 39;
pub const MAX_EXPONENT_ABS: i32 = 32_767;
pub const MAX_DECIMAL_TEXT_LEN: usize = 260;
pub const CHUNK_DIGITS: u32 = 9;
pub const CHUNK_BASE: u32 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecimalLexeme<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) span: Span32,
    pub(crate) negative: bool,
    pub(crate) integer: &'a [u8],
    pub(crate) fraction: &'a [u8],
    pub(crate) exponent: i32,
}
impl<'a> DecimalLexeme<'a> {
    pub const fn raw(&self) -> &'a [u8];
    pub const fn span(&self) -> Span32;
    pub const fn is_negative(&self) -> bool;
    pub const fn integer_digits(&self) -> &'a [u8];
    pub const fn fractional_digits(&self) -> &'a [u8];
    pub const fn exponent(&self) -> i32;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecimalBounds {
    pub min: Option<Decimal128>,
    pub max: Option<Decimal128>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccumulatedDecimal {
    pub coefficient: i128,
    pub fractional_digits: u32,
    pub exponent: i32,
    pub precision: u8,
}

pub fn validate_policy(policy: &DecimalPolicy) -> Result<()>;
pub fn coefficient_precision(coefficient: i128) -> u8;
```

Root implementations are complete. `validate_policy` accepts precision 1 through
39 and nonnegative `tick_step`; zero disables the tick constraint. Target scales
0 through 255 are valid. Invalid policies return `ConstraintViolation` at offset
zero with source `Decode`. `coefficient_precision` returns one for zero and
handles `i128::MIN` without signed negation overflow. There are no new public
traits. The crate-private helper is
`const fn decimal_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error`
and creates source `Decode`.

The canonical ABI definitions used unchanged are:

```rust
pub struct Decimal128 { pub coefficient: i128, pub scale: u8 }
pub struct DecimalPolicy {
    pub max_precision: u8,
    pub target_scale: u8,
    pub allow_exponent: bool,
    pub reject_inexact_rescale: bool,
    pub tick_step: i128,
}
```

## Wave 1: lex.rs

```rust
pub fn lex_quoted<'a>(
    input: &'a [u8],
    offset: u32,
    allow_exponent: bool,
) -> Result<DecimalLexeme<'a>>;
```

`input` is exactly one complete quoted token, including both ASCII quotes.
The complete grammar is
`'"' '-'? ('0' | [1-9][0-9]*) ('.' [0-9]+)? ([eE] [+-]? [0-9]+)? '"'`.
The exponent branch is available only when `allow_exponent` is true. No leading
plus, leading zero on a multi-digit integer, whitespace, escape sequence, Unicode
digit, omitted integer, omitted fraction digit, or trailing bytes is accepted.
There is no JSON string unescaping in this crate. Exponent leading zeros are
accepted. Its numeric absolute value must be at most `MAX_EXPONENT_ABS`, even
when its spelling has arbitrarily many leading zeros. Negative exponent zero
normalizes to zero.

The lexeme borrows integer and fraction slices containing digits only, with an
empty fraction slice when absent. `raw` borrows the entire token and `span` is
`Span32::new(offset, input.len() as u32)`. `negative` preserves lexical minus,
including negative zero. Exponent parsing is checked and never wraps.

Errors: `PayloadTooLarge` if length or absolute end cannot fit `u32`;
`UnexpectedEof` for a proper incomplete token prefix; `InvalidNumber` for every
other grammar violation, a disabled exponent, or an excessive exponent. Source
is `Lexer`. Offset is the first offending byte, or the absolute end for EOF;
an unrepresentable range reports `offset`. The entire token must be checked
before publishing a lexeme.

## Wave 2: parse.rs

```rust
pub fn accumulate(lexeme: &DecimalLexeme<'_>) -> Result<AccumulatedDecimal>;
pub fn parse_quoted(
    input: &[u8],
    offset: u32,
    policy: &DecimalPolicy,
) -> Result<Decimal128>;
```

`accumulate` concatenates integer and fractional digits arithmetically, ignoring
the point and exponent. Accumulate chunks of at most nine digits using checked
operations. Negative accumulation or an unsigned magnitude comparison must
accept the magnitude of `i128::MIN`; never construct its positive signed
magnitude. Zero becomes coefficient zero. `precision` is significant coefficient
digits after dropping leading zeros, with zero counted as one. Fractional digit
count includes every lexical fraction digit. Exponent is copied unchanged.

Errors: `NumberOverflow` for coefficient overflow, including input that would
only fit after later rescaling. Report the first digit that cannot be accumulated
at its absolute byte offset, source `Decode`. This is a deliberate bounded
coefficient contract, not arbitrary-precision parsing.

`parse_quoted` validates policy first, calls `lex::lex_quoted`, then `accumulate`,
`bounds::rescale`, and `bounds::validate` with `DecimalBounds::default()`.
It returns a value with exactly `policy.target_scale`. Policy precision is
applied to the final coefficient, not to raw leading zeros. Propagate original
errors unchanged. No float conversion, implicit saturation, or panic is allowed.

## Wave 3: bounds.rs

```rust
pub fn rescale(
    value: AccumulatedDecimal,
    policy: &DecimalPolicy,
    offset: u32,
) -> Result<Decimal128>;
pub fn validate(
    value: Decimal128,
    policy: &DecimalPolicy,
    bounds: DecimalBounds,
    offset: u32,
) -> Result<()>;
pub fn compare(left: Decimal128, right: Decimal128) -> core::cmp::Ordering;
```

`rescale` validates policy and computes mathematical source scale as
`fractional_digits - exponent` in a sufficiently wide signed type. The result
represents the same value at the target scale, except that nonzero discarded
digits may be truncated toward zero when `reject_inexact_rescale` is false.
Checked multiplication is required for increased coefficient magnitude. Very
large downward scale differences must be handled without constructing an
unrepresentable power of ten. Zero rescaling always succeeds for a valid policy.
Reject a nonzero exponent when the policy disables exponents, even for a manually
constructed `AccumulatedDecimal`. Recompute precision from the coefficient;
the public `precision` field is descriptive and must not be trusted for safety.

`validate` requires exact target-scale equality, final precision at most
`max_precision`, and inclusive numeric bounds. Bounds may have different scales.
If both exist, minimum must not exceed maximum. A positive tick requires
`coefficient % tick_step == 0` in target-scale units. Tick zero disables this
test. Negative coefficients use the same divisibility rule.

`compare` is exact for all i128 coefficients and all u8 scales. It must not
overflow by naively cross-multiplying; compare signs, decimal lengths, and
aligned digits or equivalent bounded arithmetic. It is allocation-free and
cannot fail.

Errors, source `Decode`, at the supplied `offset` except policy configuration
errors at zero: `NumberOverflow` for rescaling arithmetic overflow;
`DecimalScaleMismatch` for inexact rescaling when rejected or a scale mismatch;
`DecimalPrecisionOverflow` for final precision;
`TickMultipleViolation` for divisibility; `ConstraintViolation` for reversed
bounds, out-of-range values, invalid policy, or a disallowed exponent in a
manually constructed accumulator. Check scale, precision, range, then tick in
that order. `rescale` checks arithmetic and precision but leaves range and tick
to `validate`.

## Wave 4: write.rs

```rust
pub fn encoded_len(value: Decimal128) -> usize;
pub fn write_quoted(value: Decimal128, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_to_slice<'a>(
    value: Decimal128,
    output: &'a mut [u8],
) -> Result<&'a [u8]>;
```

Output is always quoted ASCII fixed point, with no exponent or escapes.
Scale zero has no point. Positive scales have exactly that many fractional
digits, including trailing zeros and any leading fractional zeros. Integer
part has at least one digit. A minus is written only for coefficient below zero.
Examples: `(123, 2)` becomes `"1.23"`, `(1, 4)` becomes `"0.0001"`, and
`(0, 2)` becomes `"0.00"`. Both i128 endpoints and scale 255 are supported.
`encoded_len` includes quotes and is at most `MAX_DECIMAL_TEXT_LEN`.

Format completely in a bounded local byte array, reserve the exact length once,
and call `write_bytes` once. Sink errors propagate unchanged. A sink may have its
own partial-write semantics; this crate cannot promise rollback for a failing
sink. `write_to_slice` returns just the written prefix. If the destination is
too short it returns `BudgetExhausted`, source `Encode`, offset zero, without
modifying the destination. Bytes beyond the returned prefix remain unchanged.

## Cross-wave invariants

All arithmetic is checked or proven bounded. Offsets are absolute payload byte
offsets; no narrowing cast occurs before range validation. Lexical references
are created only by the lexer and borrow their input. No wave issues a schema
validation proof. Data flows lex to parse to bounds to write; bounds never calls
parse or lex. Writers preserve coefficient and scale exactly on round trip with
a compatible policy. Detailed error text is static and contains no input data.
No implementation may introduce a dependency outside the allowed list.
