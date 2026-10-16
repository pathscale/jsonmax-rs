# jmax-native: frozen Wave 0 contract

Owner: native AOT endpoint decoders. Source of truth: `crates/jmax-native/src/lib.rs`.
Public modules: reader, context, decode. Allowed dependencies: jmax-abi, jmax-number,
jmax-decimal, jmax-string, jmax-syntax, jmax-schema-compile, jmax-schema-vm. The crate is
`#![no_std]` with `extern crate alloc`, forbids unsafe code, and supports alloc without std
through the workspace's usual `alloc`/`std` feature forwarding. It creates no thread, owns
no pool, performs no I/O, and never issues a ValidationProof.

## Purpose and non-goals

A native decoder is a generated, monomorphic, single-pass reader that turns one JSON
document directly into a typed Rust value. It is an accelerator for the schema VM, never a
second authority. The reference is the ValidatedProgram embedded next to the decoder,
lowered from the same schema graph (jmax-endpoint-model native.rs, jmax-schema-aot
compile_graph). The native path carries no proof and issues no capability. Every error
value a caller can observe comes from the schema VM or from the type's own post_check;
the native path itself only ever says "accepted" or "I do not know" (Fail).

## Root definitions and exports

Error, ErrorCode, ErrorSource, Result are canonical ABI re-exports. Decimal128,
DecimalPolicy, NumericPolicy come from jmax-abi. ValidatedProgram is the jmax-schema-compile
type; VmLimits and ValidationScratch are the jmax-schema-vm types.

```rust
pub const CONTRACT_VERSION: u16 = 1;
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fail;
pub type NativeResult<T> = core::result::Result<T, Fail>;
pub struct Reader<'a> { /* input: &'a [u8], pos: usize */ }
pub struct NativeContext { /* private */ }
pub trait NativeDecode: Sized {
    fn build_program() -> Result<ValidatedProgram>;
    fn decode_core(reader: &mut Reader<'_>, ctx: &mut NativeContext, out: &mut Self) -> NativeResult<()>;
    fn post_check(&self) -> Result<()> { Ok(()) }
}
pub fn decode_into<T: NativeDecode>(ctx: &mut NativeContext, input: &[u8], out: &mut T) -> Result<()>;
```

Fail is a zero-sized, Copy value. It carries no code, offset or text, so producing it costs
nothing and no native routine can report a wrong diagnostic. NativeContext is Debug only.
Reader is Clone + Debug.

## Wave 1: src/reader.rs

```rust
impl<'a> Reader<'a> {
    pub const fn new(input: &'a [u8]) -> Self;
    pub const fn input(&self) -> &'a [u8];
    pub const fn pos(&self) -> usize;
    pub fn skip_ws(&mut self);
    pub fn peek(&mut self) -> Option<u8>;
    pub fn eat(&mut self, byte: u8) -> bool;
    pub fn expect(&mut self, byte: u8) -> NativeResult<()>;
    pub fn end(&mut self) -> NativeResult<()>;
    pub fn boolean(&mut self) -> NativeResult<bool>;
    pub fn uint(&mut self, ctx: &mut NativeContext, max: u64) -> NativeResult<u64>;
    pub fn int(&mut self, ctx: &mut NativeContext, min: i64, max: i64) -> NativeResult<i64>;
    pub fn float(&mut self, ctx: &mut NativeContext) -> NativeResult<f64>;
    pub fn decimal(&mut self, ctx: &mut NativeContext, policy: &DecimalPolicy) -> NativeResult<Decimal128>;
    pub fn string<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c str> where 'a: 'c;
    pub fn key<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c str> where 'a: 'c;
    pub fn skip_value(&mut self, ctx: &mut NativeContext, depth: u16) -> NativeResult<()>;
    pub fn unknown_key(&mut self, ctx: &mut NativeContext) -> NativeResult<()>;
    pub fn match_key(&mut self, ctx: &NativeContext, pattern: &[u8]) -> bool;
    pub fn key_bytes<'c>(&mut self, ctx: &'c mut NativeContext) -> NativeResult<&'c [u8]>;
}
```

Every primitive skips JSON whitespace (space, tab, CR, LF) before its token, consumes
exactly one token on success, and leaves the position unchanged on Fail. Primitives that read a string or
number take the context for its limits, policies and scratch. They never allocate after
warmup: string may grow the context's decoded buffer once per capacity step, as may skip_value
its scratch.

peek returns the next non-whitespace byte without consuming it. eat consumes the byte when
it is next. expect is eat that Fails. end succeeds only when nothing but whitespace remains.
boolean accepts exactly `true` or `false` followed by a JSON value delimiter or end of input.

uint and int accept exactly the integer grammar the VM's Integer scalar accepts, with the
bounds the VM would apply: no sign other than `-`, no leading zeros, no fraction, no exponent,
a quoted spelling only when the effective numeric policy allows quoted integers (lexed
then by jmax-number exactly as the VM does), and the policy's negative-zero rule. uint accepts a nonnegative token no greater than max; int accepts any token inside
min..=max, with negative zero equal to zero when the policy admits it. A token longer than
limits.syntax.max_number_digits digits Fails. float uses jmax-number lexing and conversion
under the context's effective NumericPolicy and returns the same f64 bits. decimal accepts
a JSON string holding a fixed-point decimal and returns exactly the Decimal128 that
jmax-decimal produces for the same policy, using an inline coefficient accumulation only
when the token has at most 18 significant digits and otherwise the shared jmax-decimal
routines. Any policy bound, rescale or tick rule the shared routines would reject is a Fail.

The inline path is a pure shortcut: it reads the string straight from the input (`"`, optional `-`, `0` or a digit run not starting with `0`, optional `.` and one or more digits, `"`; at most 18 digits, no exponent, escape, whitespace or negative zero), and answers only when the value needs no downward rescale, `digits + (target_scale - fraction)` is at most 38, the coefficient is below 10^max_precision and a multiple of tick_step, the policy is one `validate_policy` accepts, and the digit and string-length limits hold. In every other case, including every rejection, it declines and the shared routines decide. A unit test checks, exhaustively over short strings and by a structured sweep over policies, that whenever it answers the shared routines return the same value.

string accepts exactly the strings BlockCursor scans: closing quote after an even run of
backslashes, no control byte below 0x20, valid UTF-8, strict escapes with paired surrogates,
and decoded length within limits.syntax.max_string_bytes. A string without escapes borrows
the input. A string with escapes is decoded once into the context's buffer through
jmax-string and borrows that buffer, so the returned reference is valid until the next
context use. The decoded buffer never exceeds the input length.

skip_value strictly validates and discards one value with jmax_syntax::skip_strict under the
context's limits, effective numeric policy and a depth supplied by the caller (the number of
enclosing containers). It accepts exactly what BlockCursor::skip_value(StrictValidate)
accepts and Fails otherwise.

key_bytes has exactly the accept set of `key` and returns the decoded key as bytes; generated decoders use it on the dispatch path because they only compare it, and a short plain-ASCII key then needs no UTF-8 check. Every string and key scan first tries an inline scan of at most 32 bytes that stops at the closing quote and gives way, to the shared scanner and with identical answers, at the first backslash, control byte, byte of 0x80 or above, or 32nd byte.

match_key supports layout prediction. `pattern` is the exact bytes `"name":` of an unescaped key; after whitespace, a match consumes the pattern and returns true, and a miss consumes only the whitespace. A pattern that is not of that form, or whose name is longer than limits.syntax.max_string_bytes, never matches, so a hit is exactly a successful `key` returning that name and the caller may fall through to `key` on a miss with no rollback.

unknown_key reads an object key and its colon exactly as key does and records the decoded key as an unknown key of the innermost object opened with begin_object. Generated code calls it on a clone of the reader taken before key, because the &str that key returns borrows the whole context and so cannot be passed to NativeContext::unknown_key.

## Wave 2: src/context.rs

```rust
impl NativeContext {
    pub fn new(limits: VmLimits, numeric: NumericPolicy) -> Self;
    pub const fn limits(&self) -> VmLimits;
    pub const fn numeric(&self) -> NumericPolicy;
    pub fn with_program(program: &ValidatedProgram, limits: VmLimits, numeric: NumericPolicy) -> Result<Self>;
    pub fn begin_object(&self) -> usize;
    pub fn unknown_key(&mut self, key: &str) -> NativeResult<()>;
    pub fn end_object(&mut self, mark: usize) -> NativeResult<()>;
}
```

NativeContext serves one decoder type at a time (decode_into rebuilds the program when the
type changes) and reuses across calls: the requested limits and numeric policy, the
effective numeric policy for number tokens (the VM's combination of the request and the
program's schema policy, computed when the program is installed), the program's own policy
(which governs numbers inside strings, such as decimals and quoted integers), a decoded-text
buffer, a stack of unknown keys, the strict-skip scratch and its decoded buffer, and the
lazily built program with the largest input length budget_safe accepts for it. After warmup a
decode of the supported shapes performs no allocation. The VM fallback uses a fresh
ValidationScratch on every call, because retained scratch capacity counts against the VM's
scratch budget and would change its answer. begin_object returns a mark for the current
object's unknown keys, unknown_key records a decoded key of the innermost object, and
end_object(mark) Fails when two recorded keys of that object are equal and
limits.syntax.reject_duplicate_keys is set; it always discards the object's keys. decode_into discards any keys left by an earlier decode that failed inside an object before it runs decode_core; a caller driving decode_core directly bounds the same leftovers with the mark. with_program
builds a context for callers that drive a Reader directly.

## Wave 3: src/decode.rs

decode_into runs this decision flow. Let program be the lazily built T::build_program(),
which is cached in the context; an error from build_program is returned unchanged.

1. Compute `safe = jmax_schema_vm::budget_safe(program, input.len(), limits)`.
2. If not safe: run the VM first (`validate_borrowed_into`, strict, with the request's numeric
   policy). A VM error is returned unchanged. Otherwise run decode_core. A Fail here is a
   divergence and returns InternalError (Decode source, detail "native decoder diverged
   from schema VM"). Then run post_check.
3. If safe: run decode_core, which must consume the whole input (Reader::end). On success run
   post_check and return its result. On Fail run the VM with the same arguments and return
   the VM's exact Error. A VM success after a Fail is a divergence and returns the same
   InternalError. The value in `out` after any error is unspecified but valid.

safe means the VM cannot fail by exhausting its step or scratch budget or its decoded-arena
limit on this input, so the VM's only possible errors are syntax, number, limit, type and
constraint errors, all of which decode_core must reproduce as an acceptance decision. Hence
with safe inputs decode_core Ok implies the VM succeeds, and the VM succeeds implies
decode_core Ok.

The accept set of decode_core must equal the VM's. decode_core must reject (Fail): any
syntax error, trailing content, an unexpected or (with reject_unknown) unknown
key, a repeated known key when limits.syntax.reject_duplicate_keys is set (otherwise the later value wins and is checked too), a missing required key, a value of the wrong kind, an integer outside its bounds, a
string whose scalar count is outside min/max length, an array outside min/max items, a
decimal outside its policy or bounds, and any limit the VM enforces: depth (inclusive, a
root container is depth 1), fields per object (known and unknown keys together), items per
array, string bytes, number digits, and duplicate keys. It must decide exactly like the VM
and must not rely on the Fail fallback to hide a missing feature: an input the VM accepts
and decode_core Fails is a bug, surfaced to callers as InternalError, never as a wrong
success. When a case cannot be decided exactly, the generator must route that schema
through the VM-first branch or refuse it with UnsupportedSchemaFeature.

post_check carries value checks the schema cannot express, such as price greater than zero,
first update id not greater than final update id, or a constant event tag. It runs on both
routes after the document is known to be schema valid, so its result never depends on which
route accepted the input. Its errors are ordinary ABI errors (Decode source), the only
errors that do not come from the VM.

Steady state: after warmup decode_into allocates nothing for the supported shapes. Arrays
and strings in `out` are cleared and refilled so their capacity is reused.

## Errors and invariants

All dependency errors keep their source, code and offsets. The only error constructed here is
the InternalError divergence above. No function panics on any input, indexes unchecked, or
recurses without a depth bound taken from the context's limits. There is no proof,
capability or Payload in this API: the typed value is returned in `out`, not in a
ParsedMessage. A caller who needs a proof-bearing message continues to use jmax-exchange or
the VM extraction APIs.

Nothing here provides a relaxed, offset-jumping, reordering-tolerant or non-equivalent mode.
Layout prediction (a per-root expected key order, tried first with a full byte compare and
no rollback) is allowed only as an internal strategy of generated code; it must not change the
accept set, and the differential suite runs with it on and off.
