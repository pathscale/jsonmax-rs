# jmax-schema-aot: frozen Wave 0 contract

Owner: W0-ROOTS-5, row 22. Source of truth: `crates/jmax-schema-aot/src/lib.rs`.
The root declares public tables, decode, encode, and verify modules. It is
allocation-capable no_std when std is disabled. Only jmax-abi,
jmax-schema-compile, and jmax-schema-ir are allowed dependencies. No source
emission API runs a compiler, formats code, or writes files.

## Root definitions and exports

The root publicly re-exports these exact ABI types, without redefining them:
ValidationProof; Error, ErrorCode, ErrorSource, Result; Decimal128; FieldId,
InsnWord, KeyEntry, SchemaFieldIr, SchemaId, SchemaImage, SchemaImageHeader,
SchemaIr, SlotValue, ValidatedSlots, Opcode, StateId; Span32, SpanSource, StrRef; ScalarKind;
ByteSink, JsonDecode, JsonEncode, SchemaProvider. CompiledProgram, CompilerOptions,
and ValidatedProgram are public re-exports from jmax-schema-compile.

```rust
pub const CONTRACT_VERSION: u16 = 1;
pub const FINGERPRINT_OFFSET: u64 = 0xcbf29ce484222325;
pub const FINGERPRINT_PRIME: u64 = 0x100000001b3;
pub struct StaticSchemaImage<'a> {
    pub header: SchemaImageHeader,
    pub insns: &'a [InsnWord],
    pub keys: &'a [KeyEntry],
    pub key_bytes: &'a [u8],
}
pub struct ImageIdentity {
    pub magic: u32, pub version: u16, pub schema_id: SchemaId, pub fingerprint: u64,
}
pub enum SlotKind { Bool, U64, I64, F64, Decimal, Str, Raw }
pub struct SlotSpec {
    pub index: u32, pub field_id: FieldId, pub kind: SlotKind, pub required: bool,
}
pub struct DecodePlan<'a> { pub schema_fingerprint: u64, pub slots: &'a [SlotSpec] }
pub struct EncodeField<'a> { pub wire_name: &'a str, pub slot: SlotSpec, pub omit_empty: bool }
pub struct TableOptions<'a> { pub symbol: &'a str, pub abi_path: &'a str, pub aot_path: &'a str }
pub struct SlotReader<'a> { values: &'a ValidatedSlots }
```

All these types are Clone + Copy + Debug; ImageIdentity, SlotKind, and SlotSpec
also derive PartialEq + Eq. SlotReader's owner field is private. Converting
`&'a SchemaImage` with `From` borrows all arrays and copies the header.
`TableOptions<'static>::default()` uses JMAX_SCHEMA, ::jmax_abi, ::jmax_schema_aot.

```rust
impl<'a> SlotReader<'a> {
    pub fn new(values: &'a ValidatedSlots, expected_fingerprint: u64) -> Result<Self>;
    pub fn values(&self) -> &'a ValidatedSlots;
    pub fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue>;
    pub fn resolve_str(&self, value: StrRef) -> Result<&'a str>;
    pub fn resolve_raw(&self, span: Span32) -> Result<&'a [u8]>;
}
pub fn fingerprint_schema(schema: &SchemaIr) -> Result<u64>;
pub fn compile_schema(schema: &SchemaIr) -> Result<ValidatedProgram>;
pub fn load_program(bytes: &[u8]) -> Result<ValidatedProgram>;
pub fn dump_program(program: &ValidatedProgram) -> Result<alloc::vec::Vec<u8>>;
pub fn rust_string_literal(text: &str) -> alloc::string::String;
pub fn validate_symbol(symbol: &str) -> Result<()>;
pub fn validate_rust_path(path: &str) -> Result<()>;
pub fn ordered_field_ids(schema: &SchemaIr) -> alloc::vec::Vec<FieldId>;
pub const RUST_KEYWORDS: &[&str];
pub(crate) fn aot_error(code: ErrorCode, source: ErrorSource, detail: &'static str) -> Error;
```

SlotReader::new checks exact fingerprint and proof payload/decoded byte lengths.
It does not claim to revalidate every slot. Each subsequent accessor checks its
own bounds; resolve_str checks UTF-8 and scalar_len in the correct owner buffer.
resolve_raw checks only bounds and does not certify a JSON fragment. Invalid
owner/index/span/scalar count is SchemaImageInvalid; invalid UTF-8 is InvalidUtf8.
Errors use Decode and index errors include the supplied FieldId with schema 0.
No proof is issued or strengthened. A transplanted proof is not authentication.

Symbols are ASCII identifiers, excluding `_` and the root's complete reserved
keyword list (including edition 2024 gen). `validate_rust_path` accepts optional
leading :: and nonempty ::-separated identifiers; crate/self/super components
are accepted only in legal
Rust placement of these contextual path components (crate first, self first,
super only as an initial repeated sequence, and none after an absolute ::), enforced by the root validator.
No spaces, generics, comments, raw identifiers, or expressions are accepted as
configured source paths. Invalid source parameters use ConstraintViolation,
SchemaCompile. rust_string_literal escapes quotes, slashes and control characters
using valid Rust escapes and never assumes an available raw-string delimiter.
ordered_field_ids preserves declaration order; its name does not imply sorting.

## Authoritative canonical identity and compilation

fingerprint_schema uses schema-ir::model::normalize, resolve::resolve with default
ResolveLimits, and canonical::fingerprint with default SchemaPolicy. All errors
propagate without replacement. The canonical grammar and hash are frozen in
contracts/jmax-schema-ir.md; do not introduce a second fingerprint grammar. In
particular canonical property order is sorted by UTF-8 key bytes while explicit
slot ids and FieldIds preserve declaration semantics. FINGERPRINT_OFFSET and
FINGERPRINT_PRIME are the same FNV-1a constants as schema-ir, not a separate hash.

compile_schema performs that same normalization/resolution, then calls
schema-compile::lower::compile with CompilerOptions::default(). load_program
calls schema-compile::image::decode with the same options. dump_program is its inverse:
schema-compile::image::encode under DEFAULT_MAX_IMAGE_BYTES, with no requirement that the
flat ABI projection verify, so programs lowered from nested graphs can be embedded. The full compiler
ValidatedProgram owns states, constraints, key strategies, patterns, slots,
policy, format options, and canonical schema bytes. A bare StaticSchemaImage is
only the ABI table projection and is not an executable program witness. Neither
matching a hash nor validating that projection certifies missing side tables.
The compiler's image magic is 0x58414d4a, version 1, flags 0. Its opcode_byte
mapping is authoritative: EnterObject=1 through RejectUnknown=14. StateId is a
dense state-vector index, not an instruction index. ProgramState.insns holds
instruction indices/counts. These facts override any Rust enum layout assumption.

## Wave 1: src/tables.rs

```rust
pub fn emit_tables(image: StaticSchemaImage<'_>, options: &TableOptions<'_>) -> Result<alloc::string::String>;
pub fn emit_program(program: &ValidatedProgram, options: &TableOptions<'_>) -> Result<alloc::string::String>;
pub fn to_owned(image: StaticSchemaImage<'_>) -> Result<SchemaImage>;
```

All three functions run verify::verify_structure before output. emit_tables validates
symbol and configured source paths before any interpolation. Emit deterministic
Rust source containing one `pub static <symbol>: <aot_path>::StaticSchemaImage<'static>`
with a header literal and borrowed promoted arrays of canonical ABI InsnWord,
KeyEntry, and key bytes. Render all integers with explicit safe types or inferred
struct field types; preserve every field, flag, reserved bit, and byte exactly.
Do not cast typed arrays to bytes or rely on repr(C) padding. Empty arrays are
legal. Repeated calls return identical bytes. to_owned clones arrays into a
SchemaImage after validation. Emitted source references only the configured ABI
and AOT crates. No heap allocation occurs when accessing the generated ABI projection static.
emit_program additionally emits a `pub static <symbol>_PROGRAM: &[u8]` containing
the complete compiler image::encode output, with DEFAULT_MAX_IMAGE_BYTES as the
byte ceiling. It includes the emit_tables projection in the same source. The
consumer obtains an executable witness through AOT load_program on those bytes;
that decoding allocates bounded owned side tables and validates them. No side
table may be omitted, reconstructed by guessing, or replaced with an empty value.
Validate both symbol and the derived suffix symbol before source interpolation.

## Wave 2: src/decode.rs

```rust
pub fn validate_slots(values: &ValidatedSlots, plan: &DecodePlan<'_>) -> Result<()>;
pub fn decode_bool(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<bool>>;
pub fn decode_u64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<u64>>;
pub fn decode_u32(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<u32>>;
pub fn decode_i64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<i64>>;
pub fn decode_i32(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<i32>>;
pub fn decode_f64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<f64>>;
pub fn decode_decimal(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<Decimal128>>;
pub fn decode_str<'a>(reader: &SlotReader<'a>, spec: &SlotSpec) -> Result<Option<&'a str>>;
pub fn decode_raw<'a>(reader: &SlotReader<'a>, spec: &SlotSpec) -> Result<Option<&'a [u8]>>;
```

validate_slots first builds a SlotReader, then requires a plan whose unique
indices cover exactly 0..values.slots.len(), with unique field ids. It checks all
slots, required values, selected types, finite floats, string spans, and raw span
bounds. Empty optional slots are permitted. Plan errors are SchemaImageInvalid;
Empty required is MissingRequiredField; wrong SlotValue is TypeMismatch;
non-finite float is InvalidNumber; narrowing overflow is NumberOverflow.

Each decoder requires a spec with its appropriate SlotKind (U64 for u32, I64 for
i32); a wrong spec kind is SchemaImageInvalid, even for Empty. Each checks the
index and field id context, then returns None only for non-required Empty. No
signed/unsigned/decimal/string coercion. F64Bits must describe a finite f64.
Decimal128 is returned unchanged. Span/UTF-8 checking delegates to SlotReader.
Errors have Decode source and the spec's FieldId; schema_id is 0 unless a caller
adds known schema context. Raw byte access makes no JSON-validity promise.

## Wave 3: src/encode.rs

```rust
pub fn write_json_string(value: &str, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_bool(value: bool, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_u64(value: u64, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_i64(value: i64, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_f64(value: f64, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_decimal(value: Decimal128, sink: &mut dyn ByteSink) -> Result<()>;
pub fn write_slots(reader: &SlotReader<'_>, fields: &[EncodeField<'_>], sink: &mut dyn ByteSink) -> Result<()>;
```

Strings use JSON quotes, short escapes where available and \u00xx for other
U+0000..U+001F; all other UTF-8 bytes remain unchanged. Integers use exact base-ten
spelling, including i64::MIN. Floats use a round-tripping finite decimal spelling;
reject NaN/infinity before writing that number. Preserve negative zero. Decimal
uses coefficient and scale exactly, including scale 0, negative i128::MIN, and
zero at any u8 scale. Emit an unquoted JSON number, never a lossy float conversion.
Use unsigned absolute magnitude to handle i128::MIN safely.

write_slots preflights duplicate keys, duplicate indices/field ids, all slot
accesses/types/requiredness and finite values before emitting the object. Fields
are emitted in supplied order. Empty optional slots are omitted if omit_empty,
otherwise encoded as null. Empty required is always an error. Raw fields are
UnsupportedSchemaFeature before output because a bounded span alone does not
certify a JSON fragment; callers needing raw emission must use a separately
validated fragment API outside this crate. Nonempty values never disappear.
Duplicate keys use DuplicateObjectKey. All local errors use Encode with field
context; original sink errors are propagated unchanged. A sink failure may leave
a prefix, so these APIs make no transactional sink guarantee.

## Wave 4: src/verify.rs

```rust
pub fn verify_structure(image: StaticSchemaImage<'_>) -> Result<()>;
pub fn verify_image(image: StaticSchemaImage<'_>, expected: ImageIdentity) -> Result<()>;
pub fn verify_fingerprint(image: StaticSchemaImage<'_>, expected: u64) -> Result<()>;
pub fn assert_provider<P: SchemaProvider>(image: StaticSchemaImage<'_>) -> Result<()>;
```

verify_structure validates only the ABI projection: compiler magic/version/flags,
header counts versus arrays without usize truncation, nonempty instructions,
known opcode bytes via compiler::decode_opcode, zero instruction flags, valid key
byte spans/UTF-8/swar heads, globally unique key FieldIds, and exact sequential
key-byte coverage with no unowned trailing bytes, except that one schema capsule (the canonical schema bytes, their length as u32 little endian and the magic `JMXSCM02`, as jmax-schema-compile appends it to every compiled program) may follow the key names. Root/state references are
state ids. Derive the dense state count and instruction boundaries from the
compiler's exact start/end instruction templates in
contracts/jmax-schema-compile.md; reject malformed templates and bad state
references. slot_count agrees with unique dense EmitSlot arg0 operands. Do not
reject FieldId u16::MAX, which is valid; only required_bit uses that sentinel.
Do not apply slot_count bounds to arbitrary key FieldIds: unselected properties
need not own slots. Empty key spans may share offsets.

The projection omits KeyTable.first_key/key_count, so object key ownership,
per-object sorting/name uniqueness, required-bit density, PHF layout, constraint
records and pattern validation cannot be certified here. Repeated names in
different objects are legal, and a global duplicate-name check would be wrong.
Those checks require the complete program and compiler image::validate/decode.
No witness or validation proof results from these shallow projection checks.
No wave may guess missing table boundaries from key ordering or slot operands.

verify_image adds exact expected magic/version/schema_id/fingerprint comparisons.
verify_fingerprint adds exact fingerprint comparison. assert_provider constructs
P::schema_ir and calls the fallible root fingerprint_schema; it requires equality
with P::schema_fingerprint and image.header.fingerprint, matching schema_id, then
runs verify_structure. It does not reconstruct or certify absent executable side
tables. Compiler image validation is mandatory before execution. Local projection
mismatches use SchemaImageInvalid/SchemaCompile with schema/field context;
canonicalization/compilation errors propagate unchanged.

## Cross-module invariants

No wave changes the root layouts or redefines ABI structs. Tables are immutable,
borrowing is owner bounded, slot order agrees with derive field order, and no
safe API issues ValidationProof. The only allowed dependencies are the three
listed above; typed writers cannot assume an unlisted number/encode library.
Wave modules may import super::* and call sibling functions through their public
module paths. tables depends on verify; decode and encode use root SlotReader;
verify uses root types/fingerprint and never calls tables, preventing recursion.
All source strings are data until returned to the caller. Unsupported schema
semantics must be explicit errors rather than generated weaker validators.

## compile_graph

```rust
pub fn compile_graph(schema: &jmax_schema_ir::Schema) -> Result<ValidatedProgram>;
```

compile_graph lowers an already nested schema graph with the same path and options as
compile_schema: schema-ir resolve with default ResolveLimits (consuming a clone of the input,
which is never mutated), then schema-compile::lower::compile with CompilerOptions::default().
It performs the validation that resolve performs and no other normalization. Errors propagate
without replacement. A graph produced by endpoint-model schema_graph_for compiles to the
program embedded next to a native decoder, and the same graph always compiles to the same
fingerprint and image bytes.
