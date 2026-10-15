# jmax-syntax frozen Wave 0 contract

Owner: W0-ROOTS-3, row 12. Allowed dependencies: `jmax-abi`, `jmax-dispatch`,
`jmax-utf8`, `jmax-string`, `jmax-number`, `jmax-buffer`, `jmax-error`.
The public modules are `machine`, `frames`, `feed`, and `finish`. All shared
definitions belong to the root; wave modules import `super::*`.

## Shared public surface

The root re-exports canonical ABI `Payload`, `Error`, `ErrorCode`, `ErrorSource`,
`Result`, `Span32`, `SpanSource`, `StrRef`, `CursorState`, `Event`, `FramePhase`,
`Limits`, `ScalarKind`, `ScanStep`, `Scratch`, and `TokenKind`. Their definitions
and discriminants are the frozen ABI's definitions; they must not be shadowed.
There are no new public traits. The root provides these exact shared definitions:

```rust
pub const JSON_WHITESPACE: [u8; 4] = [b' ', b'\t', b'\r', b'\n'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameAction { None, PushObject, PushArray, Pop }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transition {
    pub next_phase: FramePhase,
    pub action: FrameAction,
    pub event: Option<Event>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerKind { Object, Array }

#[derive(Clone, Debug)]
pub struct Frame {
    pub(crate) kind: ContainerKind,
    pub(crate) phase: FramePhase,
    pub(crate) start_offset: u32,
    pub(crate) count: u32,
    pub(crate) seen_keys: Vec<Vec<u8>>,
}
impl Frame {
    pub const fn kind(&self) -> ContainerKind;
    pub const fn phase(&self) -> FramePhase;
    pub const fn start_offset(&self) -> u32;
    pub const fn count(&self) -> u32;
}

#[derive(Debug)]
pub struct SyntaxState {
    pub(crate) limits: Limits,
    pub(crate) input: Vec<u8>,
    pub(crate) decoded: Vec<u8>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) root_phase: FramePhase,
    pub(crate) byte_offset: u32,
    pub(crate) final_input: bool,
    pub(crate) finished: bool,
    pub(crate) failure: Option<Error>,
    pub(crate) pending: Option<PendingToken>,
}
impl SyntaxState {
    pub fn new(limits: Limits) -> Self;
    pub const fn limits(&self) -> Limits;
    pub fn raw_bytes(&self) -> &[u8];
    pub fn decoded_bytes(&self) -> &[u8];
    pub const fn is_final(&self) -> bool;
    pub const fn is_finished(&self) -> bool;
    pub fn failure(&self) -> Option<&Error>;
    pub fn state(&self) -> CursorState;
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str>;
    pub(crate) fn fail(&mut self, error: Error) -> Error;
}
#[derive(Clone, Debug)]
pub struct SyntaxOutput { pub payload: Payload, pub decoded: Vec<u8> }
```

`new` is allocation-free and starts at `RootStart`, offset zero, with no frames,
pending token, error, or EOF marker. `state().depth` counts container frames only;
root does not consume depth. Its phase is the top frame's phase, otherwise the
root phase, and is always `Some`. No external mutable access to storage is
provided. `resolve_str` range-checks the selected owner and validates UTF-8.

Internal carry is frozen as follows so boundary discovery need not rescan an
ever-growing partial string:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingKind { String, Number, Literal(TokenKind) }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingToken {
    pub kind: PendingKind,
    pub start: u32,
    pub scan_offset: u32,
    pub escaped: bool,
}
```

`start` and `scan_offset` are absolute input offsets. `scan_offset` is the first
byte not yet examined for the token boundary. In a string `escaped` is the
parity of the current consecutive backslash run. Token decoding happens only
once a complete boundary has been discovered, so retries do not duplicate arena
bytes. The crate-private root helper
`const fn syntax_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error`
uses source `Syntax`.

## Wave 1: machine.rs

```rust
pub fn transition(phase: FramePhase, token: Token) -> Result<Transition>;
pub fn is_value_start(kind: TokenKind) -> bool;
```

`transition` is pure: it never mutates a parser, allocates, or inspects unrelated
input. String tokens require `decoded: Some`; all other kinds require `None`.
Token spans include the entire token, including string quotes. Punctuation spans
are one byte. For strings, the cooked reference excludes quotes and denotes
decoded text. Invalid metadata produces `InvalidToken` at `token.span.offset`.
This low-level function assumes lexical validation was done by its caller; it
does not issue a proof or permit injecting a token into `SyntaxState`.

Exact phase transitions:

| Current phase | Accepted token | Next phase | Action/event |
| --- | --- | --- | --- |
| RootStart | any value start | RootDone | scalar or container start |
| RootDone | none | none | TrailingContent |
| ObjectStart | RBrace | ObjectStart | Pop/ObjectEnd |
| ObjectStart or ObjectKeyOrEnd | String | ObjectColon | Key |
| ObjectColon | Colon | ObjectValue | None/no event |
| ObjectValue | any value start | ObjectCommaOrEnd | scalar or container start |
| ObjectCommaOrEnd | Comma | ObjectKeyOrEnd | None/no event |
| ObjectCommaOrEnd | RBrace | ObjectCommaOrEnd | Pop/ObjectEnd |
| ArrayStart | RBracket | ArrayStart | Pop/ArrayEnd |
| ArrayStart or ArrayValueOrEnd | any value start | ArrayCommaOrEnd | scalar or container start |
| ArrayCommaOrEnd | Comma | ArrayValueOrEnd | None/no event |
| ArrayCommaOrEnd | RBracket | ArrayCommaOrEnd | Pop/ArrayEnd |

All other combinations return `InvalidSyntax`. Despite the ABI variant names,
`ObjectKeyOrEnd` and `ArrayValueOrEnd` represent positions after a comma and do
not admit closing delimiters. This distinction rejects trailing commas.

Value starts are LBrace, LBracket, String, Number, True, False, and Null.
Containers produce `PushObject` or `PushArray`; scalars use `None`. Start and
end events carry the absolute offset of the delimiter byte. Key events carry
only their cooked `StrRef`, as required by the ABI. Scalar events carry the raw
token span and the cooked reference only for strings. Parent phase advances
before pushing a child. On pop, `next_phase` describes the departing frame and
does not overwrite its parent's already advanced phase. Errors use source
`Syntax` and the token start offset.

## Wave 2: frames.rs

```rust
pub fn current_phase(state: &SyntaxState) -> FramePhase;
pub fn check_payload_growth(
    current: usize,
    additional: usize,
    limits: Limits,
) -> Result<u32>;
pub(crate) fn apply(
    state: &mut SyntaxState,
    token: Token,
    transition: Transition,
) -> Result<()>;
```

`check_payload_growth` uses checked addition and requires the total to fit both
`u32` and `max_payload_bytes`. It returns the total. Errors are `PayloadTooLarge`
with source `Syntax` and offset `min(current, u32::MAX)`.

`apply` is called only by `feed` with the result of `machine::transition` for the
current phase and exactly the same token. It handles frame pushes/pops, parent
phases, counters, and decoded-key duplicate detection. Push starts at
`ObjectStart` or `ArrayStart` with count zero. Stack length cannot exceed
`limits.max_depth`; depth zero still permits scalar roots. A container start
counts as one parent value immediately. Array count increments per value start;
object count increments per accepted key. Neither a close nor punctuation
increments a count. Validate the depth and counters before applying changes.

Object keys compare by decoded UTF-8 bytes, so `"a"` and `"\u0061"` are
duplicates. No Unicode normalization or case folding is performed. Maintain
owned key bytes only when `reject_duplicate_keys` is true. Maximum fields and
array elements are inclusive. Zero limits permit empty containers only. Generic
syntax has no schema key set; `reject_unknown_keys` is passed through but has no
effect here. Downstream schema validation owns that policy.

Errors: `DepthLimitExceeded`, `DuplicateObjectKey`, `ConstraintViolation` for
field/item counts, or `BudgetExhausted` for fallible allocation failure, at the
offending token's start. Structural state commits only after checks and required
reservations succeed. `apply` records errors using `state.fail`; the original
failure is sticky. Internal inconsistent frame operations return `InternalError`
instead of indexing or popping unchecked. Source is `Syntax`.

## Wave 3: feed.rs

```rust
pub fn push(state: &mut SyntaxState, bytes: &[u8]) -> Result<()>;
pub fn next(state: &mut SyntaxState) -> Result<ScanStep>;
```

`push` copies the entire chunk into retained input after checking growth and
reserving memory. It does not tokenize or emit events. A failed reserve appends
nothing. Empty chunks before EOF are harmless. After EOF or successful finish,
any push, including an empty one, is `InvalidSyntax` at current input end.
Every failure is sticky; a preexisting failure is returned unchanged first.

On `Emit`, `byte_offset` is exactly one byte past the complete emitted token.
`next` consumes JSON whitespace, recognizes complete lexical tokens, validates
them, and runs the machine and frames modules until it can return one emitted
event. Colons and commas do not produce events. It never drops retained input.
Strings validate UTF-8, forbid unescaped control bytes, decode escapes and paired
UTF-16 surrogates, and reject lone surrogates. Unescaped strings reference raw
content; escaped strings reference appended arena bytes. `scalar_len` counts
Unicode scalar values, not bytes or UTF-16 units. Every string, including every
key, is checked even if callers later skip its value.

Number grammar is strict JSON, without quoted numbers, leading plus, leading
zeros, missing fraction digits, or missing exponent digits. Number tokens remain
raw spans and need not fit a machine numeric type. Count all number digit bytes,
including exponent digits, against `max_number_digits`. Keywords are exactly
`true`, `false`, and `null`. Token boundaries require JSON whitespace, a
structural delimiter, or confirmed EOF; accepting a keyword/number prefix of a
longer token is forbidden. Malformed adjacent tokens are rejected.

Before EOF, an incomplete token or an exhausted chunk returns `NeedMore`, even
after a complete root followed by whitespace. Retain its `PendingToken` with a
monotonically advancing boundary scan position. Do not commit partial decoded
strings or emit partial tokens. `byte_offset` remains at the incomplete token
start while carry may scan beyond it. A number or keyword reaching chunk end
waits for a boundary or EOF. A closing quote is a complete string boundary.

Enforce `max_string_bytes` on both raw string content bytes, excluding quotes,
and decoded content bytes. Enforce the arena limit on total decoded bytes and
on u32 representability before appending. Limits are inclusive. Implement input
and arena reservations with fallible allocation. Error codes are
`InvalidToken`, `InvalidNumber`, `InvalidEscape`, `LoneSurrogate`, `InvalidUtf8`,
`ControlCharInString`, `DecodedArenaOverflow`, `ConstraintViolation` for token
length/digit limits, `BudgetExhausted`, and errors from earlier modules. Lexical
errors use source `Lexer`, UTF-8 errors `Utf8`, and framing/limit errors `Syntax`.
Offsets identify the first offending byte; a size limit reports the first byte
beyond the limit. Error text contains static descriptions only.

After EOF, the final valid number/keyword may complete at input end. Open
containers, missing values, truncated literals/numbers/escapes, or an empty
document produce `UnexpectedEof` at input end. A string without a closing quote
produces `UnclosedString` at its opening quote. An already determinable lexical
error takes precedence over EOF classification. Extra non-whitespace after a
completed root with no open frames is `TrailingContent` at its first byte.

`ScanStep::Finished` is returned only after EOF, an accepted root, no frames or
pending token, and consumption of all trailing whitespace. It sets `finished`;
subsequent `next` calls return `Finished` again. It is never used for chunk
exhaustion. Error returns call `state.fail` and do not allow later recovery.

## Wave 4: finish.rs

```rust
pub fn end_input(state: &mut SyntaxState) -> Result<()>;
pub fn finish(state: &mut SyntaxState) -> Result<()>;
pub fn into_output(state: SyntaxState) -> Result<SyntaxOutput>;
```

`end_input` marks EOF and is idempotent. It performs no parsing and never discards
events. A previous error is returned unchanged. `finish` calls `end_input`, then
drains `feed::next` through `Finished`, discarding remaining events. It validates
the complete document and trailing whitespace. It is idempotent after success.
EOF diagnostics are those frozen for `feed::next`; `NeedMore` after final input
is an internal invariant violation and becomes sticky `InternalError`.

`into_output` consumes the state, calls `finish`, moves input into
`Payload::from_vec`, and moves the decoded arena into `SyntaxOutput`. All emitted
references still address the exact same byte sequences. It neither issues
`ValidationProof` nor claims schema validity. Errors propagate unchanged.

## Cross-wave invariants

Offsets and lengths are absolute u32 values, validated before narrowing. Decoded
arena indices are independent of payload offsets and always tagged accordingly.
No safe public function exposes mutable parser internals. A root phase may be
`RootDone` while child containers are open; this does not mean parsing finished.
Phase transitions never accept a trailing comma or multiple roots. Container
ends match their own kind. All error paths avoid panic and unchecked indexing.
Parser state owns all storage, and borrowed views cannot survive a mutable feed.
No backend choice changes grammar, errors, events, or bounds behavior. A backend
is an optional optimization; portable behavior is mandatory. No schema proof is
created anywhere in this crate.
