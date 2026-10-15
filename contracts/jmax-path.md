# jmax-path frozen Wave 0 contract

Owner: W0-ROOTS-3, row 14. Allowed dependencies: `jmax-abi`, `jmax-cursor`,
`jmax-buffer`. Public modules: `parse`, `compile`, `query`, `extensions`.
All wave modules import `super::*`; shared state stays in the root. In
particular, path cannot depend on string, number, syntax, or DOM crates directly.

## Shared public surface

Canonical ABI re-exports: `Payload`, `Error`, `ErrorCode`, `ErrorSource`, `Result`,
`Span32`, `SpanSource`, `StrRef`, and `Limits`. Cursor re-exports are the exact
cursor types `Segment`, `SelectedValue`, `SelectedValues`, `SelectorEdge`,
`SelectorNode`, `SelectorPlan`, `SkipMode`, and `ValueKind`. No replacement ABI
types or new public traits are introduced.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathLimits {
    pub max_path_bytes: u32,
    pub max_segments: u32,
    pub max_key_bytes: u32,
    pub max_paths: u32,
    pub max_trie_nodes: u32,
    pub max_matches: u32,
    pub max_visits: u32,
}
impl Default for PathLimits { fn default() -> Self; }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path { pub(crate) segments: Vec<Segment> }
impl Path {
    pub fn root() -> Self;
    pub fn segments(&self) -> &[Segment];
    pub fn from_segments(segments: Vec<Segment>, limits: PathLimits) -> Result<Self>;
}
#[derive(Clone, Debug)]
pub struct CompiledPaths {
    pub(crate) paths: Vec<Path>,
    pub(crate) plan: SelectorPlan,
}
impl CompiledPaths {
    pub fn paths(&self) -> &[Path];
    pub fn plan(&self) -> &SelectorPlan;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterValue { Null, Bool(bool), String(String), Number(String) }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionSegment {
    Exact(Segment),
    Wildcard,
    FilterEq { key: String, value: FilterValue },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionPath {
    pub(crate) segments: Vec<ExtensionSegment>,
    pub(crate) limits: PathLimits,
}
impl ExtensionPath {
    pub fn segments(&self) -> &[ExtensionSegment];
    pub const fn limits(&self) -> PathLimits;
}
#[derive(Clone, Debug)]
pub struct QueryResults {
    pub(crate) payload: Payload,
    pub(crate) decoded: Vec<u8>,
    pub(crate) matches: Vec<SelectedValue>,
    pub(crate) fully_validated: bool,
}
impl QueryResults {
    pub fn payload(&self) -> &Payload;
    pub fn decoded_bytes(&self) -> &[u8];
    pub fn matches(&self) -> &[SelectedValue];
    pub const fn fully_validated(&self) -> bool;
    pub fn raw(&self, index: usize) -> Option<&[u8]>;
    pub fn string(&self, index: usize) -> Option<&str>;
}
```

The root implements all listed methods. Default limits, in field order, are
65,536 bytes, 256 segments, 4,096 key bytes, 4,096 input paths, 65,536 trie nodes,
4,096 matches, and 1,000,000 event visits. Every limit is inclusive and zero is a
real bound. `Path::root()` and `Default` contain no segments.
`from_segments` takes ownership without allocation and checks segment count and
each key's decoded byte length; `max_path_bytes` constrains textual input only.
Its limit errors are `BudgetExhausted`, source `Cursor`, offset zero. Index
segments already carry a u32 and require no numeric conversion.

Read-only result accessors are range checked and tied to the result owner;
string resolution uses the correctly tagged raw or decoded arena and validates
UTF-8. Result raw string spans include quotes, and container spans include both
delimiters. The crate-private helper
`const fn path_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error`
creates source `Cursor`.

## Wave 1: parse.rs

```rust
pub fn parse(input: &str, limits: PathLimits) -> Result<Path>;
pub fn parse_many(inputs: &[&str], limits: PathLimits) -> Result<Vec<Path>>;
```

Grammar and interpretation are fixed as follows:

* Empty text and `$` select the root. An optional initial `$` must be followed
  by a dot-key or bracket selector. Without `$`, the first component may be a
  bare key or bracket selector. A leading dot without `$` is invalid.
* A bare key is one or more Unicode scalar values. Unescaped dot ends the key;
  unescaped opening bracket starts the next selector. Closing bracket,
  backslash, dollar, asterisk, question mark, whitespace, and control characters
  cannot appear unescaped in a bare key. A backslash consumes the next entire
  Unicode scalar literally, except control characters remain forbidden. Thus
  `a\.b` is one key, `a\\b` contains a backslash, and `\*` is a literal star.
  There is no `\u` decoding in bare keys; `\u0061` denotes the key `u0061`.
* Subsequent keys use a dot followed by a nonempty bare key, or a bracket-quoted
  key. Dot followed by a bracket, a consecutive dot, and a trailing dot are
  invalid. Empty keys are available through `[""]` only.
* An array index is `[` followed by `0` or `[1-9][0-9]*`, then `]`. It is parsed
  with checked u32 accumulation. Negative indices, signs, leading zeros,
  fractional indices, and whitespace inside an index are invalid.
* A bracket-quoted key is `[` followed by a complete double-quoted JSON string,
  then `]`, with no intervening whitespace. Decode JSON escapes and surrogate
  pairs; reject malformed escapes, lone surrogates, and unescaped controls.
  Escaped punctuation, whitespace, empty keys, and Unicode keys are supported.
  Such a key is always a literal key, including `"*"` and `"?"`.
* There is no trimming or optional whitespace outside quoted keys. Repeated
  brackets such as `a[0][1]` and a key after an index such as `a[0].b` are valid.

Examples: `book.price`, `$.book.price`, `book[0].price`, `$[0]`,
`["a.b"][2]`, `a\.b`, and `[""]`. These parse to owned cursor `Segment`
values, with decoded key bytes and exact numeric indices.

Basic parsing does not accept wildcard, filter, union, slice, or recursive
descent syntax. Recognized extension introducers `[*]`, `[?`, bare `*`, `..`,
and index union/slice punctuation produce `UnsupportedSchemaFeature`. Ordinary
malformed grammar produces `InvalidSyntax`; a proper truncated prefix produces
`UnexpectedEof`. Index overflow is `NumberOverflow`. Quoted key errors use
`InvalidEscape`, `LoneSurrogate`, or `ControlCharInString` as appropriate.
Invalid UTF-8 cannot enter through `&str`; decoded keys must still be valid UTF-8.
Limit violations and fallible allocation failures use `BudgetExhausted`.

Offsets are byte offsets in the path text, at the offending byte or input end
for EOF. Errors use source `Cursor` and static detail. If input length cannot fit
u32, return `PayloadTooLarge` at zero before narrowing. Check `max_path_bytes`
before parsing; its error offset is the first disallowed input byte. Decode keys
with a bounded, fallibly reserved buffer. `parse_many` first checks `max_paths`,
parses in input order, and returns the first error without publishing partial
paths. Its parse errors retain offsets relative to that individual input.

## Wave 2: compile.rs

```rust
pub fn compile(paths: &[Path], limits: PathLimits) -> Result<CompiledPaths>;
pub fn compile_strings(inputs: &[&str], limits: PathLimits) -> Result<CompiledPaths>;
```

Recheck programmatic paths against current segment/key limits and the path count
limit. Compile a shared selector trie rooted at node zero. Insertion order is
input order, sibling edges retain first-insertion order, and new nodes receive
increasing indices. Each edge target is greater than its parent index. Prefixes
share nodes, and repeated complete paths share a terminal with distinct slots.
Slot i always denotes input path i. A root path installs its slot at node zero.
Zero paths compile to a single root node with zero slots.

The compiled object owns copies of the input paths and selector strings. It
does not borrow the caller's list. Use fallible allocation for construction and
copying, then pass the graph to `SelectorPlan::new` to enforce cursor invariants.
Do not emit separate parse passes or per-slot selector programs.

Maximum nodes is the smaller of `limits.max_trie_nodes` and
`jmax_cursor::MAX_SELECTOR_NODES`. Maximum slots is the smaller of
`limits.max_paths` and `jmax_cursor::MAX_SELECTION_SLOTS`. Even the zero-path
case needs one node. Exceeding these limits returns `BudgetExhausted`, source
`Cursor`, offset zero. Allocation failure has the same code. Propagate plan
validation errors unchanged. `compile_strings` runs `parse::parse_many` then
`compile`; it cannot silently enable extensions.

## Wave 3: query.rs

```rust
pub fn execute(
    payload: Payload,
    paths: &CompiledPaths,
    mode: SkipMode,
    limits: Limits,
) -> Result<SelectedValues>;
pub fn span(results: &SelectedValues, slot: u32) -> Option<Span32>;
pub fn raw(results: &SelectedValues, slot: u32) -> Option<&[u8]>;
pub fn string(results: &SelectedValues, slot: u32) -> Option<&str>;
```

`execute` delegates to `jmax_cursor::select::extract` using `paths.plan()`.
It preserves slot order, first-complete-match duplicate behavior, missing-slot
semantics, strict/query rules, complete owner bytes, and all errors unchanged.
An unmatched path has no span; a matched null has a span. It does not decode
numbers or strip lexical whitespace inside container spans. Accessors delegate
to the checked owner accessors; `span` obtains the selected span without
claiming it belongs to any other payload. None of these APIs creates a schema
proof. An early query result exposes its validation flag to the caller.

## Wave 4: extensions.rs

```rust
pub fn compile(
    segments: &[ExtensionSegment],
    limits: PathLimits,
) -> Result<ExtensionPath>;
pub fn parse(input: &str, limits: PathLimits) -> Result<ExtensionPath>;
pub fn query(
    payload: Payload,
    path: &ExtensionPath,
    mode: SkipMode,
    limits: Limits,
) -> Result<QueryResults>;
```

This is a deliberately bounded subset. Exact segments have the basic grammar.
A sole bare `*` component, `.*`, or `[*]` denotes `Wildcard`, which selects every
immediate array element or object member value in order. It never selects keys
as values and never recursively descends. A star inside a longer unquoted key
is invalid. Quoted/escaped stars remain literal keys.

The only filter syntax is `[?(@.KEY==LITERAL)]`. KEY follows the bare-key escape
rules and denotes exactly one immediate object member, never a nested path.
ASCII JSON whitespace is allowed immediately before and after `==` and around
LITERAL, but nowhere else in the filter punctuation. LITERAL is `null`, `true`,
`false`, a strict JSON number, or a double-quoted JSON string. No operators other
than equality, expressions, functions, regex, unions, slices, negative indices,
or recursive descent are supported. Recognized out-of-subset constructs return
`UnsupportedSchemaFeature`. Other grammar errors follow basic parse errors.

Filters apply only to immediate elements of arrays. An element must be an object
whose first occurrence of KEY has the requested scalar value. Missing members,
containers, and a different scalar kind do not match. Later duplicate members
do not override the first. Null and booleans compare by type/value; strings
compare decoded UTF-8 bytes. Numbers compare exact valid lexical bytes, so `1`,
`1.0`, and `1e0` are distinct filter values. This is intentional and avoids
implicit float rounding. `FilterValue::Number` supplied programmatically must
be checked against strict JSON number grammar during `compile`.

`compile` validates segment count, exact and filter key lengths, string filter
byte lengths against `max_key_bytes`, and numeric filter spelling length against
`max_path_bytes`. It makes owned, fallibly allocated copies and retains the
limits in the program. Invalid typed numeric literals are `InvalidNumber` at
zero. `parse` also enforces total textual path length before invoking equivalent
validation. An empty extension path selects the root.

`query` returns every distinct matching value once, ordered by starting byte
offset. Overlapping ancestor/descendant matches retain both spans. Evaluate
bounded candidates with event traversal; do not materialize a DOM or evaluate
host-language code. Delayed filter decisions may retain candidate spans or use
bounded secondary cursor scans. Every event consumed in every scan, including
secondary scans, counts against `max_visits`. Check the budget before consuming
the event that would exceed it. Limit result count to `max_matches`, rejecting
overflow with `BudgetExhausted` rather than silently truncating. No unrestricted
recursion or unbounded Cartesian candidate expansion is permitted; candidate
states also count against `max_visits` when created.

Strict mode validates the entire input through EOF. Query mode must establish
all matches within the root and may stop at the completed root without checking
unrelated trailing bytes. Its flag is true only after actual full syntax
completion, exactly as in the cursor contract. Retain all supplied payload bytes
and the decoded arena needed by matches. Propagate syntax errors unchanged;
local limit/allocation failures are `BudgetExhausted` with source `Cursor` at
the current payload byte offset. No partial list is returned on failure.

## Cross-wave invariants

Basic paths and extended paths are separate APIs; compilation never silently
changes the accepted language. Decoded keys match byte-for-byte without Unicode
normalization. Path compilation is independent of payload parsing. All returned
raw and cooked views borrow their owning result, and all offsets are range
checked before narrowing. A query result never becomes `ValidatedSlots` or a
schema validation proof. The root and cursor selector definitions are reused
without a dependency cycle or additional dependency.
