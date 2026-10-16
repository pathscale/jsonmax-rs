//! Backend-neutral scanner masks and state carried across input blocks.

use crate::error::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackendId {
    Scalar,
    Neon,
    Avx2,
    Avx512,
}

/// Scanner state retained between consecutive blocks.
/// `utf8_tail_len` identifies the occupied prefix of `utf8_tail`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ScanCarry {
    pub prev_in_string: u64,
    pub prev_escaped: u64,
    pub utf8_tail: [u8; 4],
    pub utf8_tail_len: u8,
}

/// Per-byte classifications for a block of at most 64 bytes.
/// Bit `i` corresponds to byte `i`; `valid_mask` marks the occupied lanes and
/// `valid_len` is their count. `backend` stores the selected `BackendId` as a byte.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BlockMasks {
    pub quote_mask: u64,
    pub backslash_mask: u64,
    pub delimiter_quote_mask: u64,
    pub in_string_mask: u64,
    pub colon_mask: u64,
    pub comma_mask: u64,
    pub lbrace_mask: u64,
    pub rbrace_mask: u64,
    pub lbracket_mask: u64,
    pub rbracket_mask: u64,
    pub structural_mask: u64,
    pub whitespace_mask: u64,
    pub control_mask: u64,
    pub digit_mask: u64,
    pub non_ascii_mask: u64,
    pub valid_mask: u64,
    pub valid_len: u8,
    pub backend: u8,
}

impl BlockMasks {
    /// Unescaped quote positions that act as string delimiters in this block.
    pub const fn delimiter_quotes(&self) -> u64 {
        if self.delimiter_quote_mask != 0 || self.quote_mask == 0 {
            self.delimiter_quote_mask & self.valid_mask
        } else {
            self.quote_mask & !self.in_string_mask & self.valid_mask
        }
    }

    /// Raw punctuation positions before string exclusion.
    pub const fn raw_punctuation_mask(&self) -> u64 {
        (self.colon_mask
            | self.comma_mask
            | self.lbrace_mask
            | self.rbrace_mask
            | self.lbracket_mask
            | self.rbracket_mask)
            & self.valid_mask
    }

    /// Unquoted opening container positions (`{` and `[`).
    pub const fn open_container_mask(&self) -> u64 {
        (self.lbrace_mask | self.lbracket_mask) & !self.in_string_mask & self.valid_mask
    }

    /// Unquoted closing container positions (`}` and `]`).
    pub const fn close_container_mask(&self) -> u64 {
        (self.rbrace_mask | self.rbracket_mask) & !self.in_string_mask & self.valid_mask
    }
}

pub type ClassifyFn = fn(&[u8], &mut ScanCarry) -> Result<BlockMasks>;
