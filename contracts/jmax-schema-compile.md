# jmax-schema-compile frozen contract, image version 1

The normative shared root at the end freezes all common types, constants and methods. Waves own only lower.rs, keys.rs, patterns.rs and image.rs respectively. All modules can import super::*. Dependencies are restricted to jmax-abi, jmax-schema-ir, jmax-format, jmax-number, and jmax-decimal. All code is synchronous, no_std plus alloc. Use ABI errors and image structures without redefinition.

## Wave 1: lower.rs

```rust
pub fn lower(schema: &ResolvedSchema, options: &CompilerOptions) -> Result<CompiledProgram>;
pub fn compile(schema: &ResolvedSchema, options: &CompilerOptions) -> Result<ValidatedProgram>;
```

lower runs IR policy validation and canonicalization before emitting anything. Canonical breadth-first reachable type numbering becomes dense StateId numbering, root state 0. Aliases collapse, unreachable nodes disappear, and graph recursion remains finite state edges. Construct each state's key table through keys::build; compile each distinct pattern source in first-state encounter order through patterns::compile, deduplicating identical source text. max_pattern_states bounds the sum of all pattern ops, as well as each individual program. Copy scalar constraints; map string length, object property count and array length to min_count/max_count. A known format is resolved through format registry; an unknown annotated format has no executable format but remains in canonical_schema. Version 1 format_options must equal FormatOptions::default(). policy is options.policy. canonical_schema contains exactly IR canonical bytes and header.fingerprint is their FNV-1a. Retain all ABI header counts exactly; all reserved fields/flags are zero.

Every count/addition/index conversion is checked. Limits max_states, max_insns and max_keys are inclusive global totals. FieldIds must fit u16. An object has at most 65,535 required properties because required_bit=65,535 is reserved; required bits are dense in sorted-key order. Header slot_count equals slots.len(); slots are sorted by slot and must densely cover 0..slot_count. Preserve user-assigned slots, never renumber them silently.

Slots identify a single field occurrence. Reject any slot whose containing property is reachable more than once in a possible document: array ancestry, structural recursion ancestry, or multiple incoming root paths to that property are unsupported. Count paths with saturation at two; do not enumerate recursive paths. An optional or absent selected field leaves Empty. A captured Object/Array/Any becomes its full raw value span; scalar captures use their converted kind. Repeated values cannot silently overwrite a slot. FieldIds remain globally unique even for reused type nodes.

### Executable instruction grammar

Use opcode_byte/decode_opcode, never enum casts. State insns spans use instruction indices and counts, NOT byte spans; never call Span32::slice on an instruction vector. State ranges are nonempty, disjoint, contiguous in StateId order and cover all image.insns. Every instruction has flags=0. field_id=0 except EmitSlot (which contains its property's u16 FieldId). All unused operands are exactly zero.

Any range: CheckScalarKind(arg0=state ID,arg1=255). This accepts a scalar or a complete unconstrained container. Scalar range: CheckScalarKind(arg0=state ID,arg1=ValueKind tag), followed, in this exact order when applicable, by CheckIntBounds, CheckDecimalBounds, CheckStringLen, ExecPattern, ExecFormat. ValueKind tags are Null=0, Boolean=1, Integer=2, Number=3, Decimal=4, String=5. CheckIntBounds exists if either integer bound exists; CheckDecimalBounds exists for every Decimal; CheckStringLen exists if either length bound exists. Those three checks have arg0=state ID,arg1=0. ExecPattern/ExecFormat have arg0=state ID,arg1=pattern index/FormatId discriminant respectively.

Object range: EnterObject(arg0=state ID,arg1=key table index), MatchKeySwar or MatchKeyPhf(arg0=key table index,arg1=0), RejectUnknown(arg0=state ID,arg1=0) iff the shape rejects unknowns, zero or more EmitSlot instructions in sorted property-name order, then LeaveObject(arg0=state ID,arg1=0). Each EmitSlot has arg0=slot index,arg1=target StateId and its field_id set. It is a completion action for that property, not an unconditional emission upon object entry. Object entry/leave perform member-count bounds and leave checks required bits. Array range: EnterArray(arg0=state ID,arg1=items StateId), LeaveArray(arg0=state ID,arg1=0); these perform item counts and bounds. Runtime event dispatch schedules child state ranges; instructions are not an unstructured PC stream with fall-through across states.

ABI image keys/key_bytes concatenate object tables in StateId order. Rebase each BuiltKeys entry span by its byte base, PerfectHash local indices by its key base, and KeyTable.first_key by its key base exactly once. One KeyTable exists per Object in StateId order, including empty objects. SlotSpec records the property target state and FieldId. Header magic/version/flags equal root constants, schema_id matches the input, root_state=0.

lower returns an unwitnessed program and checks image::encoded_len <= max_image_bytes. compile is exactly lower followed by image::validate. No proof or unchecked witness constructor is public.

## Wave 2: keys.rs

```rust
pub fn build(specs: &[KeySpec], options: &CompilerOptions) -> Result<BuiltKeys>;
pub fn lookup<'a>(image: &'a SchemaImage, table: &KeyTable, key: &[u8]) -> Result<Option<&'a KeyEntry>>;
pub fn validate(image: &SchemaImage, table: &KeyTable) -> Result<()>;
```

build sorts unique names by raw UTF-8 bytes. Entries retain exact names, FieldIds, target states and requiredness. Key bytes are concatenated without terminators or padding, including empty names. Spans are local to BuiltKeys.bytes. swar_head is the root helper's zero-padded little-endian first eight bytes. Required bits are dense among required entries; all optional entries have NO_REQUIRED_BIT. No duplicate names or FieldIds. FieldIds must fit u16, and total size/count must fit compile limits and u32.

If count <= swar_threshold choose Swar. Otherwise try perfect hashing with capacity equal to checked next_power_of_two(max(2,2*count)), seeds 0..max_hash_attempts in order, and index `key_hash(name,seed) & (capacity-1)`. A seed is usable only if all stored keys occupy distinct slots. Empty slots contain EMPTY_KEY_SLOT. First successful seed wins; if none succeeds, fall back to Swar. No randomized seeds or unbounded search. build's hash slots contain LOCAL entry indices; lowered table slots contain GLOBAL image indices.

lookup verifies table ranges, key-byte spans, strategy structure, and candidate indices before indexing. A candidate is matched by length, swar_head and exact complete bytes. PHF collisions for unknown keys return None, never another key. Swar searches entries in stored order. validate checks sorted unique full names, swar heads, unique FieldIds, target range only when complete program state data is available to image::validate, required-bit density, power-of-two PHF capacity, seed placement, every entry exactly once, and all other slots empty. Empty tables use Swar. An invalid raw image/table -> SchemaImageInvalid, not panic or a miss.

## Wave 3: patterns.rs

```rust
pub fn compile(pattern: &str, max_states: u32, max_steps: u32) -> Result<PatternProgram>;
pub fn validate(program: &PatternProgram, max_states: u32) -> Result<()>;
pub fn is_match(program: &PatternProgram, text: &str, steps_left: &mut u64) -> Result<bool>;
```

Implement a Thompson NFA over Unicode scalar values, not UTF-8 bytes. Supported grammar: literal Unicode scalars; concatenation; `|`; capturing-syntax groups `(expr)` without captures; `.`, `^`, `$`; postfix `*`, `+`, `?`; bracket classes with scalar ranges and optional leading `^`. Empty pattern/alternatives/groups match empty. Anchors assert absolute start/end; `$` has no special before-final-newline case. Dot matches any scalar except LF, CR, U+2028, U+2029. Escapes permit metacharacters `\\ . * + ? | ( ) [ ] ^ $ - { }`, plus n/r/t meaning LF/CR/TAB. No implicit Unicode normalization. Class ranges are sorted and merged; reversed ranges fail. Class negation excludes the specified ranges across all valid scalar values.

Counted repetitions, backreferences, lookaround, lazy/possessive quantifiers, inline modes, special groups, shorthand/property escapes and unsupported escapes fail with UnsupportedSchemaFeature. Unescaped braces indicate unsupported counted syntax. Broken groups/classes/escapes, misplaced quantifiers and repeated postfix quantifiers use ConstraintViolation. Never approximate an unsupported expression by a broader language. Use iterative bounded parser/work stacks, not recursive descent with unbounded host stack.

PatternProgram.version=PATTERN_VERSION, start is valid, ops is nonempty and <=max_states, max_steps>0. All branch targets are in bounds; scalar/range endpoints are valid Unicode scalars, with ordered disjoint merged ranges. At least one Accept is reachable. Epsilon cycles are legal but each closure visitation must deduplicate PCs. Deterministic op emission follows source order; branch left before right, append one final Accept, and do not depend on map iteration order. image validation compares compilation of canonical pattern text using this same algorithm.

is_match performs unanchored search, including the end position, by injecting the start state at each scalar boundary. No recursive/backtracking execution. Charge at least one unit per visited instruction, transition test and start injection to BOTH the program's local max_steps allowance and the caller's remaining budget. Do not reset the caller's budget. Deduplicate active states. Return Ok(false) for a valid nonmatch; the VM maps it to PatternMismatch. A malformed supplied program must return SchemaImageInvalid before any unsafe indexing. Pattern or caller budget exhaustion -> BudgetExhausted. A large valid input never changes a budget failure into a nonmatch.

## Wave 4: image.rs

```rust
pub fn validate(program: CompiledProgram, options: &CompilerOptions) -> Result<ValidatedProgram>;
pub fn encoded_len(program: &CompiledProgram) -> Result<usize>;
pub fn encode(program: &ValidatedProgram, max_bytes: u32) -> Result<Vec<u8>>;
pub fn decode(bytes: &[u8], options: &CompilerOptions) -> Result<ValidatedProgram>;
pub fn cache_key(program: &ValidatedProgram, max_bytes: u32) -> Result<CacheKey>;
```

validate is the only constructor of the private ValidatedProgram field. Check magic/version/flags, counts, root, dense states, all opcode templates/operands, complete range coverage, key ownership, required counts, scalar constraint compatibility, slots, format IDs/options, NFA programs, all graph edges and all limits. policy must equal options.policy. Version 1 accepts exactly default format options. Enforce total pattern states, byte limits and no trailing/unowned bytes in key storage. Fingerprint must equal canonical-schema FNV-1a, but this alone is insufficient.

The canonical bytes must describe the executable program. Read the IR contract's canonical grammar with bounded, checked lengths, reconstruct its reachable schema graph and policy, resolve it, and require recanonicalization to reproduce the exact supplied canonical_schema. Re-lower that graph with these CompilerOptions and compare the complete deterministic encoding against the supplied program. This verifies pattern source, compiled NFA, bounds, graph/slot mappings, annotated formats, key strategy and every instruction rather than trusting hash equality. Use lower (not compile) here to avoid runtime recursion into validate. encoded_len has no witness requirement, performs only checked size computation, and never calls validate/lower. Encode implementation must have a private non-witness serializer usable by validation without bypassing public witness construction.

### Wire grammar

Encode every root/ABI field explicitly, without native struct layout. All integers are fixed-width little endian, bool is exactly u8 0/1, strings/byte vectors are u32 byte length followed by bytes, other vectors are u32 element count followed by elements, Option is u8 0/1 then the present value. Structs use declaration order in the normative root or frozen ABI. No padding. The first record is SchemaImage: header (magic u32,version u16,flags u16,schema_id u32,root_state u32,insn_count u32,key_count u32,slot_count u32,fingerprint u64), then insns, keys, key_bytes. Header is exactly 36 encoded bytes. InsnWord is u8,u8,u16,u32,u64 (16 bytes). KeyEntry is u64,Span32(u32,u32),u16,u16,u32 (24 bytes).

Next encode CompiledProgram's remaining fields in declaration order: states,key_tables,patterns,slots,policy,format_options,canonical_schema. IDs are their inner u32. All enum tags are u8 except FormatId u16. StateKind tags Any=0,Scalar=1,Object=2,Array=3, followed by variant fields in declaration order. KeyStrategy tags Swar=0,PerfectHash=1. PatternOp tags Char=0,Class=1,Any=2,Split=3,Jump=4,AssertStart=5,AssertEnd=6,Accept=7; ranges encode as vector of u32,u32 pairs. ValueKind, SchemaPolicy and DecimalConstraints encoding exactly matches IR canonical grammar. FormatOptions uses declaration order; HexCase tags Any=0,Lower=1,Upper=2, Padding tags Required=0,Forbidden=1,Optional=2. Span32 in ProgramState.insns is u32 instruction offset,u32 instruction count. CacheKey is a returned value, not embedded in the wire stream.

decode rejects bad magic/version, unknown tags/discriminants, noncanonical booleans/options, impossible counts, truncated records, overflow, invalid UTF-8 strings, trailing bytes and inconsistent semantic contents. Bound counts against byte remainder and CompileLimits BEFORE allocation; use fallible allocation. Encoded byte length must not exceed max_image_bytes. A successful decode calls validate; it never exposes a partially trusted witness. encode enforces max_bytes and u32 length representability. encoded_len must exactly equal encode's produced length for a valid program. cache_key returns header fingerprint, IMAGE_VERSION and the complete encoded bytes. Cache equality requires all bytes, not only the fingerprint.

## Errors and common invariants

All errors use ErrorSource::SchemaCompile, offset 0 unless a wire decoder has an exact byte position, and schema/field IDs where known. Structurally invalid schemas -> ConstraintViolation; unsupported semantics, out-of-u16 FieldIds and repeated capture paths -> UnsupportedSchemaFeature; compiler limits, allocation failure, size arithmetic overflow and regex budgets -> BudgetExhausted. Malformed images/tables/programs, canonical mismatch and unsupported wire versions/tags -> SchemaImageInvalid. User-supplied resource limits still produce BudgetExhausted when exceeded. No function panics for malformed input. Repeated builds with identical graph, policy and compiler options must produce byte-identical output. A compiler fingerprint is a schema identity hint, never a cryptographic digest or proof that arbitrary bytes were validated.

## Normative shared root

```rust
//! Bounded schema programs. ABI instructions and all side tables have explicit owners.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::schema::{CompileLimits, FieldId, InsnWord, KeyEntry, Opcode,
    SchemaId, SchemaImage, SchemaImageHeader, StateId};
pub use jmax_abi::span::Span32;
pub use jmax_format::{FormatId, FormatOptions};
pub use jmax_schema_ir::{DecimalConstraints, ResolvedSchema, SchemaPolicy,
    ScalarConstraints, TypeId, ValueKind};

pub mod lower;
pub mod keys;
pub mod patterns;
pub mod image;

pub const IMAGE_MAGIC: u32 = 0x58414d4a;
pub const IMAGE_VERSION: u16 = 1;
pub const IMAGE_FLAGS: u16 = 0;
pub const NO_REQUIRED_BIT: u16 = u16::MAX;
pub const EMPTY_KEY_SLOT: u32 = u32::MAX;
pub const DEFAULT_MAX_IMAGE_BYTES: u32 = 64 * 1_024 * 1_024;
pub const PATTERN_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilerOptions {
    pub limits: CompileLimits,
    pub policy: SchemaPolicy,
    pub max_image_bytes: u32,
    pub max_pattern_steps: u32,
    pub max_hash_attempts: u32,
    pub swar_threshold: u16,
}

impl Default for CompilerOptions {
    fn default() -> Self {
        Self { limits: CompileLimits::default_limits(), policy: SchemaPolicy::default(),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES, max_pattern_steps: 1_000_000,
            max_hash_attempts: 256, swar_threshold: 8 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateKind {
    Any,
    Scalar(ValueKind),
    Object { key_table: u32, required_count: u16, reject_unknown: bool },
    Array { items: StateId },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConstraintSet {
    pub min_i64: Option<i64>,
    pub max_i64: Option<i64>,
    pub decimal: Option<DecimalConstraints>,
    /// String scalars, object member count, or array item count, by state kind.
    pub min_count: Option<u32>,
    pub max_count: Option<u32>,
    pub pattern: Option<u32>,
    pub format: Option<FormatId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramState {
    pub id: StateId,
    pub kind: StateKind,
    pub insns: Span32,
    pub constraints: ConstraintSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySpec {
    pub name: String,
    pub field_id: FieldId,
    pub required: bool,
    pub target: StateId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyStrategy {
    Swar,
    /// Slots contain global image key indices, or `EMPTY_KEY_SLOT`.
    PerfectHash { seed: u64, slots: Vec<u32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyTable { pub first_key: u32, pub key_count: u32, pub strategy: KeyStrategy }

/// Construction result has local key indices and spans, rebased once by lowering.
#[derive(Clone, Debug)]
pub struct BuiltKeys {
    pub entries: Vec<KeyEntry>,
    pub bytes: Vec<u8>,
    pub strategy: KeyStrategy,
    pub required_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternOp {
    Char { scalar: u32, next: u32 },
    Class { ranges: Vec<(u32, u32)>, negated: bool, next: u32 },
    Any { next: u32 },
    Split { first: u32, second: u32 },
    Jump { next: u32 },
    AssertStart { next: u32 },
    AssertEnd { next: u32 },
    Accept,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternProgram {
    pub version: u16,
    pub start: u32,
    pub ops: Vec<PatternOp>,
    pub max_steps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSpec { pub slot: u32, pub field_id: FieldId, pub state: StateId }

#[derive(Clone, Debug)]
pub struct CompiledProgram {
    pub image: SchemaImage,
    pub states: Vec<ProgramState>,
    pub key_tables: Vec<KeyTable>,
    pub patterns: Vec<PatternProgram>,
    pub slots: Vec<SlotSpec>,
    pub policy: SchemaPolicy,
    pub format_options: FormatOptions,
    /// Exact canonical schema identity. Fingerprints alone are not cache identities.
    pub canonical_schema: Vec<u8>,
}

/// Construction is restricted to image validation. Mutation drops the witness.
#[derive(Clone, Debug)]
pub struct ValidatedProgram { pub(crate) program: CompiledProgram }

impl ValidatedProgram {
    pub fn program(&self) -> &CompiledProgram { &self.program }
    pub fn image(&self) -> &SchemaImage { &self.program.image }
    pub fn state(&self, id: StateId) -> Option<&ProgramState> {
        self.program.states.get(id.0 as usize)
    }
    pub fn into_program(self) -> CompiledProgram { self.program }
}

/// Includes the complete deterministic encoded program to disambiguate hash collisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey { pub fingerprint: u64, pub image_version: u16, pub bytes: Vec<u8> }

/// The ABI opcode enum has no representation guarantee. Never cast it to bytes.
pub const fn opcode_byte(opcode: Opcode) -> u8 {
    match opcode {
        Opcode::EnterObject => 1, Opcode::LeaveObject => 2,
        Opcode::EnterArray => 3, Opcode::LeaveArray => 4,
        Opcode::MatchKeySwar => 5, Opcode::MatchKeyPhf => 6,
        Opcode::CheckScalarKind => 7, Opcode::CheckIntBounds => 8,
        Opcode::CheckDecimalBounds => 9, Opcode::CheckStringLen => 10,
        Opcode::ExecPattern => 11, Opcode::ExecFormat => 12,
        Opcode::EmitSlot => 13, Opcode::RejectUnknown => 14,
    }
}

pub const fn decode_opcode(byte: u8) -> Option<Opcode> {
    match byte {
        1 => Some(Opcode::EnterObject), 2 => Some(Opcode::LeaveObject),
        3 => Some(Opcode::EnterArray), 4 => Some(Opcode::LeaveArray),
        5 => Some(Opcode::MatchKeySwar), 6 => Some(Opcode::MatchKeyPhf),
        7 => Some(Opcode::CheckScalarKind), 8 => Some(Opcode::CheckIntBounds),
        9 => Some(Opcode::CheckDecimalBounds), 10 => Some(Opcode::CheckStringLen),
        11 => Some(Opcode::ExecPattern), 12 => Some(Opcode::ExecFormat),
        13 => Some(Opcode::EmitSlot), 14 => Some(Opcode::RejectUnknown), _ => None,
    }
}

/// Copy, never overread, and use little-endian packing on every platform.
pub fn swar_head(bytes: &[u8]) -> u64 {
    let mut head = [0u8; 8];
    let len = bytes.len().min(head.len());
    head[..len].copy_from_slice(&bytes[..len]);
    u64::from_le_bytes(head)
}

pub fn key_hash(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = jmax_schema_ir::FINGERPRINT_OFFSET ^ seed;
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(jmax_schema_ir::FINGERPRINT_PRIME);
    }
    hash
}

pub(crate) fn compile_error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}
```
