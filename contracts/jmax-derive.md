# jmax-derive: frozen Wave 0 contract

Owner: W0-ROOTS-5, row 21. Source of truth: `crates/jmax-derive/src/lib.rs`.
The only public Rust items are these proc macro entry points:

```rust
#[proc_macro_derive(JsonDecode, attributes(jmax))]
pub fn derive_json_decode(input: proc_macro::TokenStream) -> proc_macro::TokenStream;
#[proc_macro_derive(JsonEncode, attributes(jmax))]
pub fn derive_json_encode(input: proc_macro::TokenStream) -> proc_macro::TokenStream;
#[proc_macro_derive(JsonSchema, attributes(jmax))]
pub fn derive_json_schema(input: proc_macro::TokenStream) -> proc_macro::TokenStream;
#[proc_macro_derive(DefinitionVariant, attributes(jmax))]
pub fn derive_definition_variant(input: proc_macro::TokenStream) -> proc_macro::TokenStream;
```

`decode`, `encode`, `schema`, and `attributes` MUST remain private modules. No public
helper traits or runtime values may be exported by a proc macro crate. The crate
uses `proc_macro` and the standard library, without a token parsing dependency.
Only jmax-abi, jmax-schema-aot, and jmax-schema-ir are allowed crate dependencies.
Generated code addresses runtime types through the configured ABI and AOT paths.
The consuming crate must make those crates available and make `alloc` available
with `extern crate alloc` when its generated implementation needs owned values.
Derive expansion performs no I/O and does not run external tools.

## Shared internal definitions

All names below have `pub(crate)` visibility, including every listed field.
`TokenStream`, `Ident`, and `Span` refer to `proc_macro` types, never replacement
lookalikes. Each wave may `use super::*;`.

```rust
struct DeriveError { span: Span, message: String }
type DeriveResult<T> = core::result::Result<T, DeriveError>;
enum DeriveKind { Decode, Encode, Schema, DefinitionVariant }
enum RenameRule { Preserve, SnakeCase, CamelCase, PascalCase, ScreamingSnakeCase, KebabCase }
struct ContainerAttrs {
    abi_path: TokenStream, aot_path: TokenStream, rename_all: RenameRule,
    schema_name: Option<String>, schema_id: Option<u32>, reject_unknown: bool,
}
struct FieldAttrs {
    rename: Option<String>, skip: bool, default: bool, format: Option<String>,
    min_len: Option<u32>, max_len: Option<u32>, min_i64: Option<i64>, max_i64: Option<i64>,
}
enum ScalarType { Bool, U32, U64, I32, I64, F64, String, Decimal128 }
enum FieldType { Scalar(ScalarType), Optional(ScalarType), Skipped }
struct FieldSpec {
    ident: Ident, rust_type: TokenStream, ty: FieldType, wire_name: String,
    slot: Option<u32>, attrs: FieldAttrs, span: Span,
}
struct VariantSpec { ident: Ident, wire_name: String, value: i32, description: String, span: Span }
enum ItemShape { NamedStruct(Vec<FieldSpec>), UnitStruct, Enum(Vec<VariantSpec>) }
struct DeriveInput {
    ident: Ident, impl_generics: TokenStream, type_generics: TokenStream,
    where_clause: TokenStream, attrs: ContainerAttrs, shape: ItemShape, span: Span,
}
```

All structs/enums above are `Clone + Debug`; `DeriveKind`, `RenameRule`, and
`ScalarType` are also `Copy + PartialEq + Eq`. `RenameRule::default()` is Preserve.
`FieldAttrs::default()` is false for both booleans and None for every option.
`ContainerAttrs::default()` uses `::jmax_abi`, `::jmax_schema_aot`, Preserve, no
schema name/id override, and `reject_unknown = true`.

Root helper signatures:

```rust
impl DeriveError {
    pub(crate) fn new(span: Span, message: impl Into<String>) -> Self;
    pub(crate) fn into_compile_error(self) -> TokenStream;
}
impl core::fmt::Display for DeriveError { /* writes message verbatim */ }
pub(crate) fn absolute_path(name: &str) -> TokenStream;
pub(crate) fn rust_string(value: &str) -> String;
pub(crate) fn parse_generated(source: &str, span: Span) -> DeriveResult<TokenStream>;
```

`absolute_path` is for known, valid single identifiers, such as the default crate
names. User paths must be parsed and validated, never passed unchecked to
`Ident::new`. `rust_string` produces a Rust string literal, not JSON escaping.
`parse_generated` maps token lexer errors to DeriveError at the deriving item.
`into_compile_error` creates a span-bearing `compile_error!` with a token literal;
messages cannot inject Rust tokens.

## Wave 1: src/decode.rs

```rust
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream>;
```

Emit exactly an implementation of the canonical
`jmax_abi::traits::JsonDecode::decode_slots(&ValidatedSlots) -> jmax_abi::error::Result<Self>`.
Accept named and unit structs. Do not re-emit the input item. Preserve all generic
parameters and the where clause. Add a `Self: SchemaProvider` implementation
bound because a slot proof must match the target schema identity. For each
skipped or defaulted field add its concrete Rust type's `Default` bound; do not
impose `Default` on unrelated generic parameters.

Use `SlotReader::new(slots, <Self as SchemaProvider>::schema_fingerprint())`.
The number of slots must equal the number of non-skipped fields. Slot indices
are dense declaration order and correspond to FieldId(index). Optional Empty
becomes None; required Empty is MissingRequiredField. A non-optional defaulted
field uses Default only for Empty. Skipped fields always use Default and consume
no slot. Never substitute a default for a type, range, UTF-8, or proof error.

Map scalar kinds through the AOT decode functions frozen in its contract. U32 and
I32 use checked narrowing. Decode String by resolving the StrRef against the
slot owner and copying its decoded UTF-8. Decimal128 is the ABI type without
rescaling. Reject non-finite f64. Validate every selected slot, even when a field
will be defaulted. Return the first error in declaration order; no partial Self
escapes. No borrowed references or ownerless StrRef is returned in Self.

## Wave 2: src/encode.rs

```rust
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream>;
```

Emit the canonical
`jmax_abi::traits::JsonEncode::encode_json(&self, &mut dyn ByteSink) -> Result<()>`.
Accept named/unit structs with the same field interpretation as decoding. Emit
one JSON object, declaration-order keys, no trailing comma, and no whitespace.
Skip skipped fields; omit None optional fields; include all other fields,
including values equal to Default. Unit structs emit `{}`. Use AOT's
`encode::write_json_string` for keys/strings and typed scalar writers for values.
Never use Rust debug output as JSON. The ABI has no primitive JsonEncode blanket
implementation guarantee; generated primitive writes must call the AOT helpers.
F64 uses the finite-number writer; Decimal128 uses its exact numeric writer.
A sink error is propagated immediately and the sink may contain a valid prefix.
No proof is created by encoding.

## Wave 3: src/schema.rs

```rust
pub(crate) fn expand(input: &DeriveInput) -> DeriveResult<TokenStream>;
```

Emit the canonical SchemaProvider with these exact methods:

```rust
fn schema_ir() -> jmax_abi::schema::SchemaIr;
fn schema_fingerprint() -> u64;
```

The default schema name is the Rust identifier without `r#`. Default SchemaId is
0; the `schema_id` attribute overrides it. Names and ids for emitted fields use
the same wire names and dense indices as decoding. Required means non-optional,
non-defaulted, and non-skipped. `required_count` must fit u16; reject expansion
otherwise. Apply all format and bounds attributes without silent loss.
The macro constructs the same SchemaIr during expansion and calls AOT's fallible
`fingerprint_schema` then. A failure becomes a DeriveError. Generated
`schema_fingerprint()` returns that precomputed u64 literal, so the ABI trait
remains infallible without panics or fallback hashes. Do not hash token text,
compiler spans, type layout, or platform-sized integers.

String maps to ScalarKind::String, numeric scalars to ScalarKind::Number, and
Bool maps to ScalarKind::True. The authoritative schema-ir adapter treats both
True and False as the Boolean category, not literal-only assertions. Optional
fields are optional properties: absent is None; explicit JSON null is rejected
unless a distinct nullable schema is supplied manually. Thus Optional(Scalar)
uses that scalar kind with required=false; no nullable union is claimed.
Decimal128 schema derivation is rejected because the flat ABI IR lacks an exact
decimal policy. U32 adds 0..=u32::MAX bounds; I32 adds i32 bounds. U64 is supported
by decoding and encoding but JsonSchema rejects it: the flat Number adapter does
not impose an unsigned-integer category without lossy signed bounds. I64 always
adds i64::MIN..=i64::MAX to select the authoritative Integer category. Finite F64
uses Number with no integer bounds, and integer-bound attributes are forbidden.
Constraint
intersection must not widen the intrinsic Rust range. Errors replace the whole
expansion; never emit an approximation of an unsupported field.

## Wave 4: src/attributes.rs

```rust
pub(crate) fn parse_input(input: TokenStream, kind: DeriveKind) -> DeriveResult<DeriveInput>;
pub(crate) fn expand_definition_variant(input: &DeriveInput) -> DeriveResult<TokenStream>;
```

Parse input without external token crates. Named structs and unit structs are
accepted for Decode/Encode/Schema; DefinitionVariant accepts only fieldless
enums. Tuple structs, unions, borrowed scalar fields, nested collections, and
unsupported aliases get an explicit error. Skipped fields may have any Rust
type: preserve their tokens and assign FieldType::Skipped without inspecting
that type. Raw identifiers are preserved in Rust expressions and stripped of
`r#` for default wire names. Non-skipped scalar paths are recognized only as
primitive spellings, `String`/`alloc::string::String`/`std::string::String`, and
`Decimal128` or the configured ABI path's `number::Decimal128`. Option accepts
`Option`, `core::option::Option`, and `std::option::Option` with exactly one
supported scalar argument. No inference based on arbitrary path suffixes.

Preserve generic syntax, stripping defaults only from impl generics and emitting
parameter names alone for type generics. Retain where clauses. Parsing may not
split inside groups or nested angle brackets. No input is executed.

Container `#[jmax(...)]` keys:

- `abi_path = "path"`, `aot_path = "path"`: nonempty Rust module paths, optional
  leading `::`; path keywords crate/self/super only in legal path positions.
- `rename_all = "preserve" | "snake_case" | "camelCase" | "PascalCase" |
  "SCREAMING_SNAKE_CASE" | "kebab-case"`.
- `schema_name = "nonempty name"`, `schema_id = <u32>`, `reject_unknown = <bool>`.

Field keys: `rename = "nonempty wire key"`, `skip`, `default`,
`format = "nonempty format name"`, `min_len = <u32>`, `max_len = <u32>`,
`min_i64 = <i64>`, `max_i64 = <i64>`. Boolean flag keys accept bare presence or
`= true/false`. `skip` conflicts with every field key except `default`.
String-only attributes reject numeric fields; integer bounds reject other kinds.
Inverted bounds, duplicate keys, unknown keys, misplaced keys, and wire-name
collisions are errors. Parse and validate the whole attribute list even for a
derive that does not itself consume every attribute.

Variant keys: `rename`, `value = <i32>`, `description = "text"`. Explicit Rust
integer discriminants are accepted only if a signed/unsigned integer literal
optionally preceded by minus fits i32. `value` and a Rust discriminant cannot
both occur. First implicit value is 0; each following implicit value is the
previous value plus one, checked for overflow. Explicit resets are allowed.
Reject duplicate wire names and duplicate numeric values. No arbitrary constant
expressions, data-carrying variants, or generic DefinitionVariant enums.

DefinitionVariant emits an inherent impl containing exactly:

```rust
pub const JMAX_DEFINITION_VARIANTS: &'static [(&'static str, i32, &'static str)];
```

Its value is an array of (wire name, numeric value, description), in declaration
order. This is the bridge to canonical EnumVariantSpec; adapters explicitly copy
these tuples into ABI values. No nonexistent ABI DefinitionVariant trait is
invented. A user-defined item with that reserved associated name is a Rust
compiler collision, not silently overwritten by the macro.

Casing uses ASCII word boundaries: separators underscore/hyphen/ASCII whitespace,
lowercase-or-digit followed by uppercase, and acronym boundary before an
uppercase character followed by lowercase. Reject non-ASCII identifiers when a
case conversion is requested; Preserve and explicit UTF-8 rename remain allowed.
Empty names and duplicate output names are rejected. This is the same algorithm
as endpoint-model wire_names. Numeric parsing checks overflow before narrowing.

## Cross-module invariants and failures

The root calls parse_input once, then one selected expand function. Modules do
not call each other's expand functions or duplicate attribute parsing. The input
is immutable after parsing. No root edits are needed in later waves. All user
input errors are DeriveError at the closest available source span and become
compile_error tokens. Never panic for malformed user syntax; no placeholder
expansions, silently skipped attributes, or fallback empty implementations.

Slot index, FieldId, declaration order, wire key, default behavior, and required
status must agree across all three derives and AOT plans. AOT schema fingerprints
are the authoritative canonical IR identity described in `contracts/jmax-schema-aot.md`; schema images
must carry the same identity to be consumed by those decoders. Tokens refer to
canonical ABI definitions rather than duplicate structs. Compile-time checks
remain diagnostic-only; no runtime validation proof may be issued by a derive.
