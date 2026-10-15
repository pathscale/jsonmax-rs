# jmax-format frozen contract, version 1

Scope: Wave 0 owns shared root definitions; later waves own their named modules. The root snapshot below is normative. Validators operate on decoded UTF-8 string contents supplied as bytes, never JSON quotes or escapes. They allocate no memory, accept no surrounding whitespace, and inspect exactly the supplied slice without overreads. They return the first detected failure; index is relative to that slice, with length failures at input.len(). An unrepresentable index saturates at u32::MAX. Format failures are data, not panics.

## Wave 1: uuid_ip.rs

```rust
pub fn validate_uuid(input: &[u8]) -> FormatResult<()>;
pub fn validate_ipv4(input: &[u8]) -> FormatResult<()>;
pub fn validate_ipv6(input: &[u8]) -> FormatResult<()>;
pub fn validate_ip(input: &[u8]) -> FormatResult<()>;
```

UUID is exactly 36 ASCII bytes in 8-4-4-4-12 hex groups with hyphens at 8,13,18,23. Both hex cases, nil UUID and all version/variant bits are accepted; braces, URN prefixes and compact spelling are rejected. Invalid length gives InvalidLength, bad punctuation/hex gives InvalidByte.

IPv4 is four ASCII decimal octets separated by dots. Each has 1..=3 digits and value <=255. Leading zeros are forbidden unless the octet is exactly `0`. IPv6 accepts eight 1..=4 digit hex groups, optionally one `::` replacing at least one group. A terminal dotted IPv4 tail consumes two groups and follows the IPv4 rules. No zones, brackets, whitespace, signs, network prefix, port, or IPvFuture syntax. All-zero `::` is valid. validate_ip chooses IPv6 if any colon occurs, otherwise IPv4; it never accepts a port as an address. Malformed address structure uses InvalidByte, missing/excess total content uses InvalidLength, and leading-zero IPv4 uses NonCanonical. Implement without OS-dependent address parsing or DNS.

## Wave 2: binary.rs

```rust
pub fn validate_base64(input: &[u8], policy: Base64Policy) -> FormatResult<u32>;
pub fn validate_hex(input: &[u8], policy: HexPolicy) -> FormatResult<u32>;
```

Return decoded byte count without decoding or allocation; count overflow is Limit. Empty input represents zero bytes unless an exact hex length forbids it. Base64 uses the standard alphabet or the URL-safe alphabet exclusively according to url_safe; never mix them. Reject whitespace. Padding is only terminal, at most two '=' bytes. Required demands canonical padding to a multiple of four when a partial quantum exists; full quanta need no '='. Forbidden permits no '=' and permits unpadded lengths mod 4 of 0,2,3. Optional accepts either canonical form. Length mod 4 equal to 1 is always invalid. Require zero discarded low four bits for two data chars, or zero discarded low two bits for three data chars, whether padded or not. Bad alphabet -> InvalidByte, impossible lengths -> InvalidLength, misplaced/excess/missing/forbidden padding -> InvalidPadding, nonzero discarded bits -> NonCanonical.

Hex prefix=true requires exactly lowercase `0x`; false rejects a prefix. The prefix is excluded from decoded count. Remaining bytes must be an even number of ASCII hex digits. case=Lower/Upper restricts A..F/a..f respectively and has no effect on decimal digits or the fixed prefix. exact_bytes if present is mandatory, with checked doubled-length arithmetic. Odd/wrong length -> InvalidLength, invalid digits/prefix -> InvalidByte, forbidden letter case -> NonCanonical.

## Wave 3: identity.rs

```rust
pub fn validate_nanoid(input: &[u8], len: u8) -> FormatResult<()>;
pub fn validate_address(input: &[u8]) -> FormatResult<()>;
pub fn validate_transaction_hash(input: &[u8]) -> FormatResult<()>;
```

NanoId length must equal len, with len nonzero. Alphabet is ASCII `A-Z a-z 0-9 _ -`, with no entropy claim. Address is exactly lowercase `0x` plus 40 hex digits; transaction hash is exactly lowercase `0x` plus 64 hex digits. Both accept either hex letter case and perform lexical validation only. They do not verify a chain, checksum, signature, existence or authorization. Implement address/hash by binary::validate_hex with prefix=true, case=Any, exact_bytes=20/32. Wrong length -> InvalidLength, bad alphabet/prefix -> InvalidByte. Zero NanoId length -> InvalidLength.

## Wave 4: registry.rs

```rust
pub fn lookup(name: &str) -> Result<FormatId>;
pub fn validate(id: FormatId, input: &[u8], options: &FormatOptions) -> FormatResult<()>;
pub fn validate_named(name: &str, input: &[u8], options: &FormatOptions) -> Result<()>;
```

The complete immutable registry is the ten FormatId::name() spellings. No aliases, case folding, suffix grammar, dynamic registration or global mutable state. A name longer than MAX_FORMAT_NAME_BYTES -> BudgetExhausted; unknown/empty name -> UnsupportedSchemaFeature. Registry ABI errors use SchemaVm, byte_offset/schema_id/field_id zero. validate_named maps FormatFailure through into_error(0, SchemaVm). Callers attach their own raw token/schema/field location.

validate checks input.len() <= options.max_input_bytes before dispatch, otherwise Limit. Uuid/IP options are ignored except the input limit. Hex uses options.hex. Base64/Base64Url use their respective alphabet and options.base64_padding. NanoId uses options.nanoid_len. Address/hash always use the fixed policy above, independent of options.hex. Default options give NanoId 21, ordinary hex without prefix or fixed length, and canonical padded base64. Each validator is linear time and constant auxiliary memory; IPv6 storage is bounded by eight groups. Numeric/decimal conversion and JSON parsing are outside this registry.

## Cross-module invariants

FormatId discriminants 1..=10 and registry version are frozen for schema images. FormatOptions must participate in compiled-program encoding/cache identity. No validator issues a ValidationProof or claims ownership of input. FormatFailure::into_error attaches the raw token start exactly; a decoded index cannot be added to a raw JSON offset because escapes change lengths. Limit maps to BudgetExhausted; all other format failures map to FormatMismatch.

## Normative shared root

```rust
//! Allocation-free named format contracts for decoded UTF-8 string contents.
#![no_std]
#![forbid(unsafe_code)]

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};

pub mod uuid_ip;
pub mod binary;
pub mod identity;
pub mod registry;

pub const REGISTRY_VERSION: u16 = 1;
pub const MAX_FORMAT_NAME_BYTES: usize = 64;
pub const DEFAULT_MAX_INPUT_BYTES: u32 = 1_048_576;

/// Discriminants are stable serialized identifiers. Zero is never a format.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FormatId {
    Uuid = 1, Ipv4 = 2, Ipv6 = 3, IpAddr = 4,
    Base64 = 5, Base64Url = 6, Hex = 7, NanoId = 8,
    BlockchainAddress = 9, BlockchainTransactionHash = 10,
}

impl FormatId {
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Uuid), 2 => Some(Self::Ipv4), 3 => Some(Self::Ipv6),
            4 => Some(Self::IpAddr), 5 => Some(Self::Base64), 6 => Some(Self::Base64Url),
            7 => Some(Self::Hex), 8 => Some(Self::NanoId),
            9 => Some(Self::BlockchainAddress), 10 => Some(Self::BlockchainTransactionHash),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Uuid => "uuid", Self::Ipv4 => "ipv4", Self::Ipv6 => "ipv6",
            Self::IpAddr => "ip", Self::Base64 => "base64", Self::Base64Url => "base64url",
            Self::Hex => "hex", Self::NanoId => "nanoid",
            Self::BlockchainAddress => "blockchain-address",
            Self::BlockchainTransactionHash => "blockchain-transaction-hash",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Padding { Required, Forbidden, Optional }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HexCase { Any, Lower, Upper }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base64Policy { pub url_safe: bool, pub padding: Padding }

impl Default for Base64Policy {
    fn default() -> Self { Self { url_safe: false, padding: Padding::Required } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HexPolicy {
    pub case: HexCase,
    pub prefix: bool,
    pub exact_bytes: Option<u32>,
}

impl Default for HexPolicy {
    fn default() -> Self { Self { case: HexCase::Any, prefix: false, exact_bytes: None } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    pub max_input_bytes: u32,
    pub nanoid_len: u8,
    pub hex: HexPolicy,
    pub base64_padding: Padding,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self { max_input_bytes: DEFAULT_MAX_INPUT_BYTES, nanoid_len: 21,
            hex: HexPolicy::default(), base64_padding: Padding::Required }
    }
}

/// Failure index is relative to the decoded value, not a raw JSON byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatFailureKind { InvalidLength, InvalidByte, InvalidPadding, NonCanonical, Limit }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatFailure { pub kind: FormatFailureKind, pub index: u32 }

pub type FormatResult<T> = core::result::Result<T, FormatFailure>;

impl FormatFailure {
    pub fn new(kind: FormatFailureKind, index: usize) -> Self {
        Self { kind, index: u32::try_from(index).unwrap_or(u32::MAX) }
    }

    /// Attach the enclosing raw token's offset; decoded indices cannot be added to it.
    pub fn into_error(self, token_offset: u32, source: ErrorSource) -> Error {
        let code = if self.kind == FormatFailureKind::Limit {
            ErrorCode::BudgetExhausted
        } else { ErrorCode::FormatMismatch };
        let detail = match self.kind {
            FormatFailureKind::InvalidLength => "invalid format length",
            FormatFailureKind::InvalidByte => "invalid format byte",
            FormatFailureKind::InvalidPadding => "invalid base64 padding",
            FormatFailureKind::NonCanonical => "noncanonical format encoding",
            FormatFailureKind::Limit => "format input limit exceeded",
        };
        Error::new(code, source, token_offset, detail)
    }
}
```
