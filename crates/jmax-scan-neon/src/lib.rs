//! Bounded 64-byte JSON classification with a safe portable fallback.
//!
//! Bit zero always describes the first input byte. Public entry points must
//! detect CPU support before entering an architecture-specific function.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "std")]
extern crate std;

pub mod classify;
pub mod quotes;
pub mod structural;
pub mod tails;

pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::scan::{BackendId, BlockMasks, ClassifyFn, ScanCarry};

/// Number of byte positions represented by one ABI mask.
pub const BLOCK_BYTES: usize = 64;
/// Bytes in each native vector used by this backend.
pub const LANE_BYTES: usize = 16;
pub const VECTOR_LANES: usize = 4;
pub const BACKEND: BackendId = BackendId::Neon;
/// Stable ABI backend tag: scalar=0, NEON=1, AVX2=2, AVX-512=3.
pub const BACKEND_TAG: u8 = 1;

/// Raw lexical classes, independent of quoting and carry state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructuralMasks {
    pub colon_mask: u64,
    pub comma_mask: u64,
    pub lbrace_mask: u64,
    pub rbrace_mask: u64,
    pub lbracket_mask: u64,
    pub rbracket_mask: u64,
    pub whitespace_mask: u64,
    pub control_mask: u64,
    pub digit_mask: u64,
    pub non_ascii_mask: u64,
}

impl StructuralMasks {
    /// All six punctuation classes, before string exclusion.
    pub const fn punctuation_mask(&self) -> u64 {
        self.colon_mask | self.comma_mask | self.lbrace_mask | self.rbrace_mask
            | self.lbracket_mask | self.rbracket_mask
    }
}

/// Full-block byte classification. The backend records the path actually used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawMasks {
    pub quote_mask: u64,
    pub backslash_mask: u64,
    pub classes: StructuralMasks,
    pub backend: BackendId,
}

impl Default for RawMasks {
    fn default() -> Self {
        Self {
            quote_mask: 0,
            backslash_mask: 0,
            classes: StructuralMasks::default(),
            backend: BackendId::Scalar,
        }
    }
}

/// Quote analysis restricted to real bytes, excluding delimiter quotes from
/// the string-content mask. Escaped quotes remain string content when inside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuoteMasks {
    pub escaped_quote_mask: u64,
    pub delimiter_quote_mask: u64,
    pub in_string_mask: u64,
}

/// Whether this build can safely execute its AArch64 NEON implementation.
/// Without std, only statically enabled NEON is accepted.
pub fn is_supported() -> bool {
    #[cfg(all(feature = "std", target_arch = "aarch64"))]
    { std::arch::is_aarch64_feature_detected!("neon") }
    #[cfg(all(not(feature = "std"), target_arch = "aarch64"))]
    { cfg!(target_feature = "neon") }
    #[cfg(not(target_arch = "aarch64"))]
    { false }
}

/// Low `len` bits, including the zero-length and full-width cases.
/// Rejects lengths above 64 without shifting by 64.
pub fn valid_mask(len: usize) -> Result<u64> {
    match len {
        0 => Ok(0),
        1..=63 => Ok((1u64 << len) - 1),
        BLOCK_BYTES => Ok(u64::MAX),
        _ => Err(Error::new(
            ErrorCode::InvalidToken,
            ErrorSource::Scanner,
            0,
            "scanner block exceeds 64 bytes",
        )),
    }
}

/// Validate only the quote fields; the UTF-8 fields belong to their consumer.
pub(crate) fn validate_carry(carry: &ScanCarry) -> Result<()> {
    if carry.prev_in_string > 1 || carry.prev_escaped > 1 {
        return Err(Error::new(
            ErrorCode::InternalError,
            ErrorSource::Scanner,
            0,
            "quote carry fields must be zero or one",
        ));
    }
    Ok(())
}

impl RawMasks {
    /// Assemble an ABI block while masking away every padded byte.
    /// Callers supply quote masks calculated with the same real byte length.
    pub(crate) fn finish(self, quotes: QuoteMasks, len: usize) -> Result<BlockMasks> {
        let valid = valid_mask(len)?;
        let c = self.classes;
        let in_string = quotes.in_string_mask & valid;
        let backend = match self.backend {
            BackendId::Scalar => 0,
            BackendId::Neon => 1,
            BackendId::Avx2 => 2,
            BackendId::Avx512 => 3,
        };
        Ok(BlockMasks {
            quote_mask: self.quote_mask & valid,
            backslash_mask: self.backslash_mask & valid,
            in_string_mask: in_string,
            colon_mask: c.colon_mask & valid,
            comma_mask: c.comma_mask & valid,
            lbrace_mask: c.lbrace_mask & valid,
            rbrace_mask: c.rbrace_mask & valid,
            lbracket_mask: c.lbracket_mask & valid,
            rbracket_mask: c.rbracket_mask & valid,
            structural_mask: c.punctuation_mask() & !in_string & valid,
            whitespace_mask: c.whitespace_mask & valid,
            control_mask: c.control_mask & valid,
            digit_mask: c.digit_mask & valid,
            non_ascii_mask: c.non_ascii_mask & valid,
            valid_mask: valid,
            valid_len: len as u8,
            backend,
        })
    }
}

/// Canonical safe ABI entry point, accepting zero through 64 bytes.
pub fn classify(input: &[u8], carry: &mut ScanCarry) -> Result<BlockMasks> {
    classify::classify(input, carry)
}
