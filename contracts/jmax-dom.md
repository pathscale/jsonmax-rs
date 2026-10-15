# jmax-dom frozen Wave 0 contract

Owner: W0-ROOTS-3, row 15. Allowed dependencies: `jmax-abi`, `jmax-cursor`,
`jmax-buffer`, `jmax-string`, `jmax-number`. Public modules: `build`, `navigate`,
`scalar`, `rebuild`. All wave modules use `super::*`; shared layout stays in the
root. No dependency on path, syntax, decimal, or a general JSON DOM is permitted.

## Shared public surface

The root re-exports the canonical ABI `Payload`, `Error`, `ErrorCode`,
`ErrorSource`, `Result`, `Container32`, `DocumentParts`, `NodeWord`,
`ValidatedDocument`, `ConvertedNumber`, `NodeId`, `Span32`, `SpanSource`,
`StrRef`, `Limits`, and `ByteSink`. No ABI wrapper with a conflicting layout is
introduced. There are no new public traits.

```rust
pub const NO_NODE: u32 = u32::MAX;
pub const TAG_NULL: u8 = 0;
pub const TAG_FALSE: u8 = 1;
pub const TAG_TRUE: u8 = 2;
pub const TAG_NUMBER: u8 = 3;
pub const TAG_STRING: u8 = 4;
pub const TAG_ARRAY: u8 = 5;
pub const TAG_OBJECT: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeKind { Null, Bool, Number, String, Array, Object }
impl NodeKind { pub const fn from_tag(tag: u8) -> Option<Self>; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    pub parent: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub key: Option<StrRef>,
    pub key_span: Option<Span32>,
    pub string: Option<StrRef>,
    pub number_index: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct Document {
    pub(crate) payload: Payload,
    pub(crate) parts: DocumentParts,
    pub(crate) metadata: Vec<NodeMeta>,
}
impl Document {
    pub fn payload(&self) -> &Payload;
    pub fn parts(&self) -> &DocumentParts;
    pub fn metadata(&self) -> &[NodeMeta];
    pub fn node_count(&self) -> usize;
    pub fn resolve_str(&self, reference: StrRef) -> Option<&str>;
}
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    pub(crate) document: &'a Document,
    pub(crate) id: NodeId,
}
impl<'a> Node<'a> {
    pub const fn id(self) -> NodeId;
    pub const fn document(self) -> &'a Document;
}
#[derive(Clone, Copy, Debug)]
pub struct Object<'a> { pub(crate) node: Node<'a> }
impl<'a> Object<'a> { pub const fn node(self) -> Node<'a>; }
#[derive(Clone, Copy, Debug)]
pub struct Array<'a> { pub(crate) node: Node<'a> }
impl<'a> Array<'a> { pub const fn node(self) -> Node<'a>; }

#[derive(Clone, Debug)]
pub struct Children<'a> {
    pub(crate) document: &'a Document,
    pub(crate) next: Option<NodeId>,
    pub(crate) remaining: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct Member<'a> { pub key: &'a str, pub value: Node<'a> }
#[derive(Clone, Debug)]
pub struct Members<'a> { pub(crate) children: Children<'a> }
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scalar<'a> { Null, Bool(bool), Number(&'a [u8]), String(&'a str) }

#[derive(Clone, Debug)]
pub enum EditAction {
    Replace(Payload),
    Remove,
    AppendArray(Payload),
    SetMember { key: String, value: Payload },
}
#[derive(Clone, Debug)]
pub struct Edit { pub target: NodeId, pub action: EditAction }
```

All listed root methods are implemented. `NodeKind::from_tag` returns `None` for
unknown tags and maps both boolean tags to `Bool`. `resolve_str` range-checks
and validates UTF-8 against the owner chosen by `SpanSource`. `Document` exposes
read-only storage only; public `NodeMeta` values cannot be injected into a
document. Cloning a document retains an independent immutable decoded arena and
a clone of the payload owner, preserving every offset. Node handles borrow one
specific document. No public function accepts a detached NodeWord as a handle.
The private helper
`const fn dom_error(code: ErrorCode, offset: u32, detail: &'static str) -> Error`
creates source `Dom`.

## Flat layout invariants

These rules are local to this crate's `Document`. Do not assume an arbitrary
external `ValidatedDocument` already has this DOM layout.

* One value produces one node, in preorder. Root is always `NodeId(0)` and the
  document is nonempty. Object keys are metadata attached to their value node;
  they never appear as separate nodes. Total nodes cannot exceed `u32::MAX`,
  leaving `NO_NODE` unavailable as a real node index.
* `parts.nodes`, `parts.spans`, and `metadata` have identical lengths. Span i is
  the full raw value span of node i, including quotes for strings and both
  delimiters for containers. Whitespace internal to a container remains within
  its original raw span. Whitespace outside root is not part of root's span.
* All NodeWords have `aux24() == 0`. Scalar tags 0 through 4 use
  `payload_index() == node_id.0`, indexing their raw span. Container tags 5 and 6
  use `payload_index()` to index `parts.containers`. Boolean value is encoded
  only in its true/false tag. Unknown tags never enter a built document.
* Container records have `reserved == 0`, `child_count` equal to immediate
  values, and `end_node` equal to the exclusive preorder end of that subtree.
  Empty containers have `first_child == NO_NODE`; otherwise it is the next
  preorder node after the container. For objects, child count is field count,
  not twice the field count. Sibling links skip each preceding child's entire
  subtree. They end in `None` after exactly `child_count` values.
* Root has no parent, key, key span, or sibling. Every other node has a valid
  preceding container parent. Object children have both key and key span;
  array children have neither. Key spans include quotes; their cooked refs
  exclude quotes. Only string value nodes have `string: Some`.
* All payload spans resolve in the retained payload. Decoded StrRefs resolve in
  `parts.decoded`; unescaped text references raw content. Scalar counts in
  StrRefs are accurate Unicode scalar counts. Builder retains the cursor arena
  with exactly the same offsets; no arena rebasing may be implicit.
* Parsing leaves `parts.numbers` empty and `number_index` absent. JSON numbers
  remain exact lexical bytes, including huge finite decimal spellings that do
  not fit a machine integer or float. A future cache may be added only by an
  explicit contract revision, with validated indices and equivalence rules.

## Wave 1: build.rs

```rust
pub fn parse(payload: Payload, limits: Limits) -> Result<Document>;
pub fn from_validated(document: ValidatedDocument, limits: Limits) -> Result<Document>;
```

`parse` uses a strict forward cursor, constructs flat nodes and metadata, and
drains through EOF before publishing the document. Allocate node, span, metadata,
container, and construction-stack storage with fallible reservations. Apply
canonical syntax limits including duplicate-key policy. Container starts reserve
their node and container record, and matching ends close raw spans, child counts,
and exclusive subtree ends. Strings and keys retain the exact cooked references
emitted by the cursor. Link siblings and parents as nodes are committed.

ABI key events do not include a raw token span. Recover it from the cursor's
retained bytes between the pre-advance committed offset and its post-key
committed offset: skip validated whitespace and separators to the opening
quote, then use the committed end as the span end. A key event can only occur in
an object-key position; do not reinterpret arbitrary decoded arena offsets as
payload positions. Recovering a span must never unescape or scan unbounded
external memory. Validate both ends against the retained input.

After successful cursor finalization, retain its payload and decoded owners.
Retaining the supplied immutable payload owner instead of the cursor's byte copy
is allowed if every byte is identical and all offsets are unchanged. Publish
only after the flat-layout invariants above hold. There are no half-built public
documents and no externally constructible `Document` literal.

`from_validated` consumes an ABI document and reparses its payload under the
supplied limits using `parse`. Its old parts are not assumed to use this layout,
and its schema proof is not copied, fabricated, or reissued. This explicitly
validates both foreign and ABI documents at the DOM boundary.

Errors: propagate cursor/syntax errors unchanged, including trailing content;
`PayloadTooLarge` for unrepresentable node/span/container indices;
`BudgetExhausted` for allocation failure; `InternalError` for impossible event
order or invalid internal references. Local errors use source `Dom` and the
current absolute payload offset. No numeric overflow occurs merely from storing
the raw spelling of a syntactically valid number.

## Wave 2: navigate.rs

```rust
pub fn root(document: &Document) -> Node<'_>;
pub fn node(document: &Document, id: NodeId) -> Option<Node<'_>>;
pub fn kind(node: Node<'_>) -> NodeKind;
pub fn raw_span(node: Node<'_>) -> Span32;
pub fn parent<'a>(node: Node<'a>) -> Option<Node<'a>>;
pub fn children<'a>(node: Node<'a>) -> Result<Children<'a>>;
pub fn object<'a>(node: Node<'a>) -> Result<Object<'a>>;
pub fn array<'a>(node: Node<'a>) -> Result<Array<'a>>;

impl<'a> Object<'a> {
    pub fn len(self) -> usize;
    pub fn is_empty(self) -> bool;
    pub fn get(self, key: &str) -> Option<Node<'a>>;
    pub fn iter(self) -> Members<'a>;
}
impl<'a> Array<'a> {
    pub fn len(self) -> usize;
    pub fn is_empty(self) -> bool;
    pub fn get(self, index: usize) -> Option<Node<'a>>;
    pub fn iter(self) -> Children<'a>;
}
impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Self::Item>;
    fn size_hint(&self) -> (usize, Option<usize>);
}
impl<'a> ExactSizeIterator for Children<'a> {}
impl<'a> core::iter::FusedIterator for Children<'a> {}
impl<'a> Iterator for Members<'a> {
    type Item = Member<'a>;
    fn next(&mut self) -> Option<Self::Item>;
    fn size_hint(&self) -> (usize, Option<usize>);
}
impl<'a> ExactSizeIterator for Members<'a> {}
impl<'a> core::iter::FusedIterator for Members<'a> {}
```

Root and kind accessors are infallible because handles originate only from a
valid immutable document. `node` returns `None` for every out-of-range index,
including `NO_NODE`. `parent(root)` is `None`. Children of arrays and objects
are immediate value nodes, in order, without keys as nodes. Scalar `children`
and mismatched `object`/`array` conversions return `TypeMismatch` at the node's
raw start with source `Dom`.

Object lookup compares decoded UTF-8 bytes and returns the first duplicate key
when duplicates were allowed by parsing. Iteration preserves all duplicates.
Array lookup is zero based and returns `None` out of range. Navigation never
allocates or reparses the payload. Iterators follow sibling links, stop exactly
at their stored remaining count, and remain exhausted after their first `None`.
The exact-size count and size hint are the remaining immediate child count.

## Wave 3: scalar.rs

```rust
pub fn raw<'a>(node: Node<'a>) -> Result<&'a [u8]>;
pub fn value<'a>(node: Node<'a>) -> Result<Scalar<'a>>;
pub fn string<'a>(node: Node<'a>) -> Result<&'a str>;
pub fn boolean(node: Node<'_>) -> Result<bool>;
pub fn u64(node: Node<'_>) -> Result<u64>;
pub fn i64(node: Node<'_>) -> Result<i64>;
pub fn f64(node: Node<'_>) -> Result<f64>;
pub fn converted(node: Node<'_>) -> Result<ConvertedNumber>;
```

`raw` works on any node, including containers, and returns its exact raw span.
`value` accepts scalar nodes only, returning cooked text for strings and raw
bytes for numbers. `string` and `boolean` require their exact node kind.
Numeric accessors require Number; quoted numbers are not implicitly converted.
Integer accessors require an integer spelling without point or exponent, perform
checked conversion, and accept `-0` as signed zero. Unsigned conversion rejects
any minus spelling, including `-0`, with `ConstraintViolation`. A nonintegral
spelling requested as integer is `InvalidNumber` even if its numeric value is
integral. Integer range failures are `NumberOverflow`.

`f64` performs correctly rounded binary64 conversion and rejects nonfinite
results with `NumberOverflow`; finite underflow and signed zero are valid.
`converted` chooses U64 for unsigned integer spellings, I64 for minus integer
spellings, and F64Bits for decimal/exponent spellings. It does not fall back to
float when integer conversion overflows and never guesses a decimal policy.
Repeated conversions do not mutate storage or populate a hidden cache.

Type mismatch errors are `TypeMismatch`. Unresolvable internal spans/text use
`InternalError`. Local errors have source `Dom` and the node raw start. If a
dependency supplies a more precise numeric error, preserve its byte position
after translating a token-relative offset to the node's absolute position;
the source may remain that dependency's source. No raw access is lost when a
numeric conversion fails. String access never allocates.

## Wave 4: rebuild.rs

```rust
pub fn apply(document: &Document, edits: &[Edit], limits: Limits) -> Result<Document>;
pub fn compact(document: &Document, limits: Limits) -> Result<Document>;
pub fn write_document(document: &Document, sink: &mut dyn ByteSink) -> Result<()>;
```

Edits reference node IDs in the original document, never IDs produced by earlier
edits. Validate every target before writing. Multiple edits targeting the same
node, or edits with ancestor/descendant targets, are `ConstraintViolation`.
Reject such conflicts before applying any operation. Out-of-range targets use
`ConstraintViolation` at zero. Caller order never resolves conflicts implicitly.

`Replace` installs exactly one complete JSON value from its owned payload.
`Remove` removes an array element or complete object member, including its key;
removing root is `ConstraintViolation`. `AppendArray` requires an array target
and appends one validated value. `SetMember` requires an object target; it
replaces the first matching decoded key's value, preserving that member's key
spelling and position, or appends a newly escaped key and value if absent.
Other duplicate members are retained unless the final limits reject them.
Wrong container kinds are `TypeMismatch` at the target raw start.

Validate every supplied replacement/append/member value as one strict complete
JSON value, rejecting trailing content. Check all replacement input limits and
the final document limits, including combined depth, field/item counts, payload
bytes, and decoded arena size. An invalid new key or over-limit key must fail,
not be truncated. Keys are Rust strings and must be escaped as valid JSON.

Serialize the resulting tree in document order with no insignificant whitespace,
using an explicit bounded traversal stack rather than recursive calls on
untrusted depth. Preserve raw scalar number, string, boolean, and null spelling
and existing key spelling. Inserted keys use valid deterministic JSON escaping:
quote and backslash use their two-byte escapes, backspace/form-feed/newline/
carriage-return/tab use short escapes, other U+0000 through U+001F use lowercase
`\u00xx`, and all remaining scalars use UTF-8 without escaping slash. Containers
use ordinary delimiters, colons, and commas with no trailing comma.

`apply` builds a new payload and calls `build::parse` under the final limits.
The original document and its handles remain unchanged. New node IDs, spans,
decoded offsets, and references belong exclusively to the returned document;
IDs are not stable across rebuilds. Empty edits still produce a compact rebuild.
`compact` is equivalent to applying no edits. Its payload contains only the
rebuilt root bytes, and its arena contains only reachable decoded strings, with
all references rebuilt. It retains no unused old input owner through a hidden
back-reference.

`write_document` uses the same compact serialization, preserves exact scalar
spellings, and performs no edits or proof issuance. Sink failures propagate
unchanged; a failing sink may already have received a prefix. Use checked size
arithmetic, bounded temporary storage, and fallible allocation. New payload
growth beyond `limits.max_payload_bytes` returns `PayloadTooLarge` before
further growth; allocation failures use `BudgetExhausted`. For a provided sink,
its own `reserve`/write limits determine sink capacity.

Local edit/serialization errors use source `Dom` and the original target raw
start where available. Replacement parse errors keep offsets relative to that
replacement payload. Final assembled parse errors keep offsets relative to the
new payload. Static details distinguish these stages without leaking input.
Every failure returns no new document and leaves the original untouched.

## Cross-wave invariants

Raw ownership and decoded ownership are inseparable from document views. A
safe document is always complete syntactically valid JSON with the frozen flat
layout. Duplicate policy is decided at construction; lookup does not change it.
No wave mutates an existing document, forges a schema proof, assumes unvalidated
external layout, converts all numbers to floats, or uses unsafe code. Every
fallible construction either publishes a complete valid owner or returns an
error. Canonical NodeWord/Container32/DocumentParts types remain unchanged.
