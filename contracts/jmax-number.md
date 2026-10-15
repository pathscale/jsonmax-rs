# jmax-number: frozen Wave 0 contract

Status: frozen for Waves 1 through 4. Allowed dependencies: jmax-abi and
jmax-error. No dependency or manifest change is authorized. All public APIs are
safe and support no-std without allocation. Unsafe code is forbidden. Decimal128
fixed-point conversion belongs to jmax-decimal, not this crate.

## Canonical ABI and exact root definitions

```rust
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::{ConvertedNumber, NumericPolicy};
pub use jmax_abi::span::{NumberLexKind, Span32};
pub use jmax_abi::traits::ByteSink;
pub const MAX_U64_TEXT_BYTES: usize = 20;
pub const MAX_I64_TEXT_BYTES: usize = 20;
pub const MAX_F64_TEXT_BYTES: usize = 768;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumberToken<'a> {
    bytes: &'a [u8],
    span: Span32,
    kind: NumberLexKind,
    negative: bool,
    integer_digits: u32,
    fraction_digits: u32,
    exponent: i32,
}
impl<'a> NumberToken<'a> {
    pub const fn raw_bytes(&self) -> &'a [u8];
    pub const fn span(&self) -> Span32;
    pub const fn kind(&self) -> NumberLexKind;
    pub const fn is_negative(&self) -> bool;
    pub const fn integer_digits(&self) -> u32;
    pub const fn fraction_digits(&self) -> u32;
    pub const fn exponent(&self) -> i32;
    pub const fn is_quoted(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct NumberText {
    bytes: [u8; MAX_F64_TEXT_BYTES],
    len: usize,
}
impl NumberText {
    pub const fn new() -> Self;
    pub fn as_bytes(&self) -> &[u8];
    pub const fn len(&self) -> usize;
    pub const fn is_empty(&self) -> bool;
    pub fn clear(&mut self);
}
impl Default for NumberText { fn default() -> Self; }
impl core::fmt::Write for NumberText {
    fn write_str(&mut self, text: &str) -> core::fmt::Result;
}
pub const fn is_number_delimiter(byte: u8) -> bool;
pub(crate) fn checked_input_len(input: &[u8]) -> Result<u32>;
```

All NumberToken and NumberText fields are private, accessible inside descendant
modules. Only lex constructs NumberToken; there is no public unchecked
constructor or mutable token accessor. The token borrows its exact bytes and
never points at synthesized or stack-temporary text. No new public enum, error
type, or trait is introduced.

NumberToken.bytes is the numeric spelling, including a minus sign if present,
but excluding optional wrapper quotes. span covers the full source token,
including wrapper quotes. Integer/fraction digit counts exclude signs, decimal
point, exponent and exponent digits. exponent is the signed explicit exponent,
zero if absent, and is not adjusted for fraction_digits. is_quoted is exactly
kind == NumberLexKind::QuotedDecimal.

NumberText is scratch storage, not lexical evidence. New/default length is
zero; clear resets length. write_str appends a complete UTF-8 string if it fits,
or returns fmt::Error without changing existing bytes or length. All arithmetic
is checked and capacity is never exceeded. A larger formatting operation may
have appended earlier pieces before a later write_str fails; callers discard
that local scratch on a formatting error. Byte slices expose only initialized,
logically written bytes.

is_number_delimiter accepts only space, tab, CR, LF, comma, right bracket and
right brace. It rejects colon, opening containers, all letters, slash and
non-ASCII bytes. EOF is handled by lex rather than this byte predicate.
checked_input_len rejects lengths above u32::MAX with PayloadTooLarge, source
Lexer, offset u32::MAX, before any narrowing.

NumericPolicy remains the canonical ABI record, including the exact
allow_Quoted_int spelling. Do not substitute a normalized local policy type.
A negative max_exponent_abs is an invalid caller policy: ConstraintViolation at
the token start, or offset zero for output. Schema and field IDs retain the
canonical Error::new defaults unless inherited unchanged from a sink.

## Wave 1: lex.rs

```rust
pub fn lex_number<'a>(
    input: &'a [u8],
    offset: u32,
    policy: &NumericPolicy,
) -> Result<NumberToken<'a>>;
```

Validate the whole input length, then policy, then scan from exactly offset;
no whitespace is skipped. An offset at or beyond EOF is UnexpectedEof at offset.
Accept precisely the JSON number grammar:

```text
number   = minus? integer fraction? exponent?
minus    = '-'
integer  = '0' | [1-9][0-9]*
fraction = '.' [0-9]+
exponent = [eE] [+-]? [0-9]+
```

Reject leading plus, leading zero followed by another integer digit, missing
integer/fraction/exponent digits, hexadecimal syntax, underscores, whitespace
within the number, NaN and Infinity. After a candidate token require EOF or an
is_number_delimiter byte. A colon, slash, quote, letter, or other invalid suffix
must cause InvalidNumber rather than acceptance of the preceding prefix. A
number boundary does not establish that the containing JSON syntax is valid;
the syntax owner validates commas and container closure separately.

If policy.allow_Quoted_int is true, additionally accept an opening double quote,
an otherwise strict optionally negative integer, and its closing double quote.
No fraction, exponent, escape, plus, or internal whitespace is accepted in this
form. Require the same post-token boundary after the closing quote. Store kind
QuotedDecimal, even though this policy admits only integer spelling; raw_bytes
excludes quotes and span includes them. A disabled quoted form is InvalidNumber
at its opening quote. A missing closing quote after an otherwise complete quoted
integer is UnexpectedEof at EOF. A malformed quoted digit sequence retains the
ordinary InvalidNumber rules.

Unquoted negative integer spelling has kind SignedInt; nonnegative integer
spelling has kind UnsignedInt. Any fraction or exponent gives FloatDecimal,
including 1e0. Lexing never imposes a u64 or i64 magnitude bound. Huge integer
magnitudes are valid tokens to be converted or rejected by the chosen converter.

Accumulate exponent magnitude without wrap, comparing significant digits to
policy.max_exponent_abs. Arbitrarily many leading exponent zeros remain valid.
An exponent outside the allowed magnitude returns ConstraintViolation at its
first exponent digit; do not wrap an intermediate accumulator. If
reject_negative_zero is true, a negative token whose integer and fraction
digits are all zero returns ConstraintViolation at its minus sign, irrespective
of the exponent. Complete grammar and boundary validation precedes these value
policy checks. No fixed digit cap, stack-sized input copy, or arbitrary precision
allocation is needed. Higher layers enforce Limits.max_number_digits.

All other lexical failures are InvalidNumber at the first offending byte; an
incomplete required grammar component at EOF uses InvalidNumber at input.len().
For example 01 fails at 1, 1x at x, and 1e+ at EOF. All read-side errors in this
crate use ErrorSource::Lexer and absolute source offsets. No partial token is
returned on an error; the input and caller state remain untouched.

## Wave 2: integer.rs

```rust
pub fn parse_u64(token: &NumberToken<'_>) -> Result<u64>;
pub fn parse_i64(token: &NumberToken<'_>) -> Result<i64>;
pub fn convert_integer(token: &NumberToken<'_>) -> Result<ConvertedNumber>;
```

Accept UnsignedInt, SignedInt and the integer-only QuotedDecimal spelling. Reject
FloatDecimal with InvalidNumber at token.span().offset, even if its numeric value
is integral. Tokens already encode the lexical policy under which they were
created; these functions add no implicit policy defaults.

parse_u64 rejects every negative token, including allowed lexical -0, with
NumberOverflow at the token start. For nonnegative tokens accept exactly
0..=u64::MAX. parse_i64 accepts exactly i64::MIN..=i64::MAX, including the full
negative minimum without negating i64::MIN or first overflowing an i64
accumulator. Negative zero becomes integer zero if lexing allowed it.
convert_integer uses I64 for a negative token, U64 otherwise, and never falls
back to f64 on range overflow. Positive values above i64::MAX still fit U64 when
within u64::MAX. A quoted token follows its sign, not the wrapper kind, for the
ConvertedNumber variant.

Use checked accumulation, optionally eight ASCII digits at a time with SWAR.
Validate chunk bounds before reading; from_le_bytes on copied fixed arrays is
permitted. Never trust platform alignment or native byte order, or use wrapping
multiply-add to mask overflow. A range failure is NumberOverflow at token start,
source Lexer. Inputs, tokens and output destinations are never mutated.

## Wave 3: float.rs

```rust
pub fn parse_f64(token: &NumberToken<'_>, policy: &NumericPolicy) -> Result<f64>;
pub fn convert_float(
    token: &NumberToken<'_>,
    policy: &NumericPolicy,
) -> Result<ConvertedNumber>;
```

Accept integer, decimal and permitted quoted-integer tokens. Recheck the supplied
policy, including quoted-integer permission, explicit exponent bounds and
negative zero; a token lexed under a looser policy must not bypass the current
one. Policy errors use the same offsets and codes as lex. parse_f64 converts
the complete decimal spelling to IEEE-754 binary64 with correctly rounded
round-to-nearest, ties-to-even semantics, including long significands, subnormal
numbers, exact halfway cases, and signed zero. Do not approximate with integer
casts followed by repeated powers of ten or rely on a bounded prefix without a
correct fallback. A safe core str parser is an allowed exact fallback and
requires no external conversion dependency. All numeric spelling bytes are
ASCII by the token invariant; use safe str conversion.

Finite underflow rounds to zero or a subnormal and is accepted. Preserve the
negative sign of an underflowed zero unless policy.reject_negative_zero, in
which case return ConstraintViolation at the minus sign. When conversion
produces infinity, reject it with NumberOverflow at token start if
reject_non_finite_float is true; otherwise return the signed infinity. Grammar
never admits a NaN or Infinity literal, and NaN cannot result from conversion
of a valid decimal. A safe parser failure on a token is InvalidNumber at token
start rather than a panic. Conversion errors use source Lexer.

convert_float wraps the exact parse_f64 result as ConvertedNumber::F64Bits using
to_bits, preserving signed zero and any permitted infinity. It never chooses an
integer or Decimal variant, and does not alter the token or its backing bytes.

## Wave 4: write.rs

```rust
pub fn write_u64(value: u64, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_i64(value: i64, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_f64(
    value: f64,
    policy: &NumericPolicy,
    sink: &mut dyn ByteSink,
) -> Result<()>;
```

Integer functions emit minimal ordinary base-10 ASCII with no leading plus,
leading zeros, quotes, whitespace or terminator byte. Zero is 0; i64::MIN must
format correctly without signed negation overflow. Their maximum appended
lengths are the root 20-byte constants. They accept all values of their types.

write_f64 emits a valid JSON number which parses back to the identical finite
binary64 bits, including the sign of allowed negative zero. Use shortest
round-trip significant decimal digits with deterministic ordinary or scientific
notation; no specific choice between these two notations is required. Scientific
exponents use lowercase e and canonical integer digits without redundant leading
zeros; a plus sign is optional for positive exponents. Core's round-trip float
formatting is allowed, with a core::fmt::Write scratch adapter; no truncation or
lossy fixed decimal precision is allowed. Negative zero must include its minus
sign. No decimal point is required for integral values.

Every NaN or infinity is rejected with NumberOverflow, source Encode, offset zero,
even when reject_non_finite_float is false: JSON has no legal spelling for such
a value. reject_negative_zero causes ConstraintViolation, source Encode, offset
zero for negative zero. A negative max_exponent_abs is ConstraintViolation at
zero before other value checks. allow_Quoted_int does not alter numeric output.
The exponent bound applies to any explicit output exponent; choose ordinary
notation when required to satisfy it. All finite binary64 values fit the bounded
scratch in ordinary notation, including subnormals, so a finite value is not
rejected merely for requiring this notation. Max exponent zero is therefore
well-defined.

Format and validate the complete spelling in local stack storage before any
sink call. Scratch overflow or impossible formatter failure is InternalError,
source Encode, offset zero, without touching the sink. Reserve exactly the
complete output length, then write it; sink errors propagate unchanged and a
write failure may leave a prefix. No global buffer, heap allocation, byte-sink
implementation, or alternate error abstraction is required.

## Cross-module invariants

- Token construction belongs to lex; all fields, positions, grammar kind and
  borrowed bytes agree. No wave adds public token constructors or mutators.
- Every read-side error is canonical Error with source Lexer, every synthesized
  write-side error uses source Encode. Sink errors retain all original metadata.
- Checked spans refer to original input, including quoted wrappers. Parsed
  values never replace raw spans needed for later exact-decimal processing.
- Integer conversion is exact or an error. Floating conversion explicitly
  rounds to binary64 and never silently changes an integer converter's result.
- Exponent, sign, digit counts, byte lengths and sink lengths never truncate,
  wrap, overflow a signed minimum, or depend on host endianness.
- NumberText is shared root storage; module implementations import super::*
  rather than defining competing token, policy, result or scratch types.
- Dependency direction is lex -> root; integer -> root; float -> root and
  safe core parsing; write -> root and core formatting. No inter-wave type or
  recursive module dependency is necessary.

The root declares separately owned wave modules. Wave 0 does not create those
files, execute any toolchain command, or claim build/execution validation.
