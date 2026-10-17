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
    /// Nonempty, ASCII `A-Z a-z 0-9 _ -`; any length (a schema bounds it separately).
    NanoIdChars = 11,
    /// Nonempty, ASCII `A-Z a-z 0-9`; any length (a schema bounds it separately).
    Base62 = 12,
}

impl FormatId {
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Uuid), 2 => Some(Self::Ipv4), 3 => Some(Self::Ipv6),
            4 => Some(Self::IpAddr), 5 => Some(Self::Base64), 6 => Some(Self::Base64Url),
            7 => Some(Self::Hex), 8 => Some(Self::NanoId),
            9 => Some(Self::BlockchainAddress), 10 => Some(Self::BlockchainTransactionHash),
            11 => Some(Self::NanoIdChars), 12 => Some(Self::Base62),
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
            Self::NanoIdChars => "nanoid-chars", Self::Base62 => "base62",
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
