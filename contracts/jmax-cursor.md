# jmax-cursor frozen Wave 0 contract

Owner: W0-ROOTS-3, row 13. Allowed dependencies: `jmax-abi`, `jmax-syntax`,
`jmax-string`, `jmax-number`, `jmax-buffer`. Public modules: `walk`, `skip`,
`select`, `resume`. Wave files import the root using `super::*`. No module
depends on `jmax-path` or a DOM; selector definitions live here to avoid cycles.

## Shared public surface

Canonical ABI re-exports: `Payload`, `UnvalidatedSpan`, `Error`, `ErrorCode`,
`ErrorSource`, `Result`, `Span32`, `SpanSource`, `StrRef`, `CursorState`, `Event`,
`Limits`, `ScalarKind`, and `ScanStep`. `SyntaxState` is re-exported from
`jmax-syntax`. There are no new public traits.

```rust
pub const MAX_SELECTOR_NODES: usize = 65_536;
pub const MAX_SELECTION_SLOTS: u32 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipMode { Strict, Query }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ValueKind { Object, Array, String, Number, Bool, Null }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedValue {
    pub kind: ValueKind,
    pub span: Span32,
    pub decoded: Option<StrRef>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipStep {
    NeedMore,
    Complete { value: SelectedValue, validated: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment { Key(String), Index(u32) }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorEdge { pub segment: Segment, pub target: u32 }
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectorNode { pub edges: Vec<SelectorEdge>, pub slots: Vec<u32> }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorPlan {
    pub(crate) nodes: Vec<SelectorNode>,
    pub(crate) slot_count: u32,
}
impl SelectorPlan {
    pub fn new(nodes: Vec<SelectorNode>, slot_count: u32) -> Result<Self>;
    pub fn nodes(&self) -> &[SelectorNode];
    pub const fn slot_count(&self) -> u32;
}

#[derive(Debug)]
pub struct Cursor {
    pub(crate) syntax: SyntaxState,
    pub(crate) mode: SkipMode,
    pub(crate) current: Option<Event>,
    pub(crate) skipping: Option<SkipProgress>,
}
impl Cursor {
    pub fn new(limits: Limits, mode: SkipMode) -> Self;
    pub const fn mode(&self) -> SkipMode;
    pub fn state(&self) -> CursorState;
    pub const fn current(&self) -> Option<Event>;
    pub fn raw_bytes(&self) -> &[u8];
    pub fn decoded_bytes(&self) -> &[u8];
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str>;
}
#[derive(Debug)]
pub struct SuspendedCursor { pub(crate) cursor: Cursor }
#[derive(Clone, Debug)]
pub struct CursorOutput {
    pub payload: Payload,
    pub decoded: Vec<u8>,
    pub fully_validated: bool,
}
#[derive(Clone, Debug)]
pub struct SelectedValues {
    pub(crate) payload: Payload,
    pub(crate) decoded: Vec<u8>,
    pub(crate) slots: Vec<Option<SelectedValue>>,
    pub(crate) fully_validated: bool,
}
impl SelectedValues {
    pub fn payload(&self) -> &Payload;
    pub fn decoded_bytes(&self) -> &[u8];
    pub fn slots(&self) -> &[Option<SelectedValue>];
    pub const fn fully_validated(&self) -> bool;
    pub fn raw(&self, slot: u32) -> Option<&[u8]>;
    pub fn string(&self, slot: u32) -> Option<&str>;
}
```

Root methods have complete implementations. `SelectorPlan::new` requires one to
65,536 nodes and at most 65,536 slots. Root is node zero. Every edge target must
be strictly greater than its source index and within the graph. All nodes must
be reachable. Each node's edge segments must be unique. Every slot in
`0..slot_count` occurs exactly once among all terminal slot lists; multiple
different slots on one terminal are permitted. Sharing a target across parents
is permitted if all invariants hold. Invalid plans return `ConstraintViolation`;
validation allocation failure returns `BudgetExhausted`, both at zero with source
`Cursor`. The plan owns all key strings and exposes no mutable graph access.
Segment ordering is enum declaration order (Key before Index), then UTF-8
lexical key order or numeric index order. Validation sorts temporary borrowed
edge keys to detect duplicates without quadratic sibling comparisons; it
preserves edge insertion order in the owned graph.

`Cursor::new` is allocation-free. `current` is the last emitted event and is
initially `None`. Result accessors return `None` for missing/out-of-range slots
or invalid spans. `string` also requires string kind and valid cooked UTF-8.
Raw string results retain their quotes. Containers retain both delimiters.
There is no numeric conversion in cursor extraction.

Internal skip state and helper are frozen:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SkipProgress {
    pub start: u32,
    pub kind: ValueKind,
    pub depth: u16,
    pub mode: SkipMode,
}
pub(crate) const fn cursor_error(
    code: ErrorCode, offset: u32, detail: &'static str,
) -> Error;
```

`depth` is unmatched containers within the skipped subtree, starting at one.
The root helper creates source `Cursor`. All spans use canonical ABI types.

## Wave 1: walk.rs

```rust
pub fn from_payload(payload: Payload, limits: Limits, mode: SkipMode) -> Result<Cursor>;
pub fn push(cursor: &mut Cursor, bytes: &[u8]) -> Result<()>;
pub fn end_input(cursor: &mut Cursor) -> Result<()>;
pub fn next(cursor: &mut Cursor) -> Result<ScanStep>;
```

`from_payload` creates a cursor, copies payload bytes through syntax `feed::push`,
and marks EOF with syntax `finish::end_input`. It does not consume events or
claim that syntax has already been validated. The input owner may then be
dropped because the parser owns its byte copy. Chunked callers instead use
`Cursor::new`, `push`, and `end_input`. Empty input is diagnosed when advanced or
finished, not by construction.

`push` and `end_input` delegate to syntax and retain its sticky errors and EOF
rules. `next` delegates to syntax `feed::next`, updating `current` only on `Emit`.
On `NeedMore` or `Finished`, `current` remains unchanged. Event order is JSON
document order, with keys before values and exactly balanced container events
on successful full traversal. Cursor maintains no node tape or tree.

While a subtree skip is suspended, ordinary `next` returns `InvalidSyntax` at
the current cursor offset and leaves both parser and skip progress unchanged.
`push`, `end_input`, and suspension are still allowed. This API-use error is
recoverable; resume by calling `skip::subtree` again. Underlying syntax failures
remain sticky and propagate unchanged. Forward traversal cannot rewind or emit
an event twice.

## Wave 2: skip.rs

```rust
pub fn subtree(cursor: &mut Cursor, mode: SkipMode) -> Result<SkipStep>;
```

On its first call this consumes the subtree whose scalar or start event is
already `cursor.current()`. A key, end event, or absent current event is
`TypeMismatch` at the current byte offset, without advancing the cursor. Scalars
return immediately with their complete raw token span and optional cooked
reference. Container starts install `SkipProgress` and consume subsequent events
directly from the syntax parser, tracking nested starts/ends until the matching
end. Their completed span starts at the opening delimiter and ends just after
the closing delimiter. Enforce checked span and depth arithmetic.

At chunk exhaustion, return `NeedMore` and retain progress inside the cursor.
Subsequent calls continue that same skip; the mode argument must match the
stored mode or return recoverable `ConstraintViolation` without advancing.
Changing chunks must not restart a subtree or lose nesting. On completion,
clear progress and leave `current` at the final scalar or container end. A
repeated call after a container completion sees an end event and is a type
error, rather than replaying the completed skip. A repeated scalar skip may
return the same scalar because no advancement was necessary.

Strict mode validates the entire skipped subtree, including strings, numbers,
UTF-8, duplicate keys, and all syntax limits, returning `validated: true`.
Query mode may use the same strict syntax engine, but must return
`validated: false` conservatively. This flag describes the subtree result,
not a completed document or schema proof. Query does not require scanning
unrelated suffix bytes. Both modes must recognize the correct subtree boundary
and must never trust quote or nesting bytes without handling string escapes.

Underlying errors propagate unchanged. EOF inside the subtree is
`UnexpectedEof`; reaching `Finished` with unmatched skip depth is `InternalError`.
No mode may turn malformed consumed bytes into a validated result. Subtree
completion alone never marks the document fully validated.

## Wave 3: select.rs

```rust
pub fn extract(
    payload: Payload,
    plan: &SelectorPlan,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues>;
```

Execute the complete selector graph over one forward event traversal. Prefixes
are shared; do not reparse the whole payload separately for every slot. A key
edge matches decoded object key bytes exactly, with no normalization. An index
edge matches an immediate array element at its zero-based index. Segment/container
type mismatch means no match, not an error. A terminal at root selects the
entire root value, including its complete container span. Prefix and descendant
terminals may both capture spans from the same traversal.

Allocate exactly `slot_count` entries, initially `None`. A slot records the first
complete matching value in document order and is not overwritten by duplicate
members. If an earlier duplicate member lacks the remaining path, a later
complete match may fill the slot. Missing paths stay `None`; JSON null is a
present `SelectedValue` with `ValueKind::Null`. Multiple slots on one terminal
receive the same value. Determine container spans only at their matching end;
never return an opening-delimiter-only span. String scalar references are copied
unchanged into the result and resolved against the result owner.

Strict mode traverses through final EOF even after all slots fill. Return
`fully_validated: true` only after successful syntax completion. Query mode may
stop once all slots fill, but otherwise must scan to EOF to establish absence.
It may also choose to complete validation. The flag is true only when EOF was
actually fully validated. With zero slots, strict still validates the entire
document; query may return immediately with false. Partially validated results
never produce `ValidationProof` or `ValidatedSlots`.

Use fallible reserves for slot and traversal storage. Errors include all syntax
errors, `PayloadTooLarge`, `BudgetExhausted`, and `InternalError` for impossible
plan traversal state. Syntax errors retain their source/offset; local errors use
source `Cursor` and the current absolute input offset. Failed extraction returns
no partial result. Output payload bytes must equal the entire supplied payload,
even if query stopped early. The arena retains every referenced decoded string.

## Wave 4: resume.rs

```rust
pub fn suspend(cursor: Cursor) -> SuspendedCursor;
pub fn resume(suspended: SuspendedCursor) -> Cursor;
pub fn finish(cursor: Cursor) -> Result<CursorOutput>;
pub fn into_partial(cursor: Cursor) -> Result<CursorOutput>;
```

Suspend/resume are infallible ownership moves. They preserve current event,
pending token, skip progress, input, arena, stack, limits, failure, EOF marker,
and mode exactly. Neither type is `Clone`. No references into caller-provided
chunks are retained. No IO, scheduling, or background execution occurs.

`finish` marks EOF, completes any active skip using its stored mode, and drains
the parser to `Finished`. It consumes the cursor and transfers the syntax
`finish::into_output` owners into `CursorOutput` with `fully_validated: true`.
It always validates all remaining bytes, including in query mode. Syntax errors
propagate unchanged. Successfully finishing an already finished cursor works.

`into_partial` is available only on a query cursor. On a strict cursor it returns
`ConstraintViolation`. It returns any existing parser failure unchanged and
rejects an active incomplete skip with `TransportIncomplete`. Otherwise it
retains the entire fed input and decoded bytes without advancing parsing. Since
syntax does not expose move-out of incomplete buffers, use fallible copies of
its read-only byte accessors; do not change the syntax root. Set the flag from
`syntax.is_finished()`, never from root phase or EOF marking alone. Incomplete
unconsumed suffix bytes may be present and are explicitly unvalidated.

## Cross-wave invariants

No cursor mode manufactures a schema proof. A true `fully_validated` flag means
complete JSON syntax validation of that owner only. All storage outlives every
stored span and cooked reference. Every externally returned borrowed byte/string
view is tied to its owner. Selection and skipping share the canonical event
stream and syntax limits. External borrowed selectors cannot enter suspended
state. API misuse errors do not silently advance traversal. Checked allocation
failures map to `BudgetExhausted`; error detail strings contain no payload data.
