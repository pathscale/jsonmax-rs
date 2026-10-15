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

pub type ClassifyFn = fn(&[u8], &mut ScanCarry) -> Result<BlockMasks>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ErrorCode, ErrorSource};

    #[test]
    fn default_carry_starts_outside_strings_without_pending_utf8() {
        let carry = ScanCarry::default();
        assert_eq!(carry.prev_in_string, 0);
        assert_eq!(carry.prev_escaped, 0);
        assert_eq!(carry.utf8_tail, [0; 4]);
        assert_eq!(carry.utf8_tail_len, 0);
    }

    #[test]
    fn carry_copy_preserves_all_pending_state() {
        let carry = ScanCarry {
            prev_in_string: u64::MAX,
            prev_escaped: 1,
            utf8_tail: [0xf0, 0x9f, 0x92, 0],
            utf8_tail_len: 3,
        };
        let mut copy = carry;
        assert_eq!(copy, carry);
        copy.utf8_tail[2] = 0x98;
        assert_ne!(copy, carry);
        assert_eq!(carry.utf8_tail[2], 0x92);
    }

    #[test]
    fn default_masks_have_no_valid_or_classified_bytes() {
        let masks = BlockMasks::default();
        assert_eq!([
            masks.quote_mask, masks.backslash_mask, masks.in_string_mask,
            masks.colon_mask, masks.comma_mask, masks.lbrace_mask, masks.rbrace_mask,
            masks.lbracket_mask, masks.rbracket_mask, masks.structural_mask,
            masks.whitespace_mask, masks.control_mask, masks.digit_mask,
            masks.non_ascii_mask, masks.valid_mask,
        ], [0; 15]);
        assert_eq!(masks.valid_len, 0);
        assert_eq!(masks.backend, BackendId::Scalar as u8);
    }

    #[test]
    fn full_width_masks_preserve_high_bit_and_backend() {
        let masks = BlockMasks {
            quote_mask: 1 << 63,
            backslash_mask: 1 << 62,
            in_string_mask: u64::MAX,
            colon_mask: 1 << 61,
            comma_mask: 1 << 60,
            lbrace_mask: 1 << 59,
            rbrace_mask: 1 << 58,
            lbracket_mask: 1 << 57,
            rbracket_mask: 1 << 56,
            structural_mask: 0xff << 56,
            whitespace_mask: 1 << 55,
            control_mask: 1 << 54,
            digit_mask: 1 << 53,
            non_ascii_mask: 1 << 52,
            valid_mask: u64::MAX,
            valid_len: 64,
            backend: BackendId::Avx512 as u8,
        };
        let copy = masks;
        assert_eq!(copy, masks);
        assert_eq!(copy.quote_mask >> 63, 1);
        assert_eq!(copy.valid_len, 64);
        assert_eq!(copy.valid_mask, u64::MAX);
    }

    #[test]
    fn backend_tags_follow_declaration_order() {
        assert_eq!(BackendId::Scalar as u8, 0);
        assert_eq!(BackendId::Neon as u8, 1);
        assert_eq!(BackendId::Avx2 as u8, 2);
        assert_eq!(BackendId::Avx512 as u8, 3);
        fn assert_hash<T: core::hash::Hash>() {}
        assert_hash::<BackendId>();
    }

    #[test]
    fn classifier_signature_supports_state_and_shared_errors() {
        // This test classifier accepts empty input only, modeling an end-of-input
        // boundary while retaining pending state for the owning scanner.
        let classify: ClassifyFn = |input, carry| {
            if !input.is_empty() {
                return Err(Error::new(
                    ErrorCode::InvalidToken, ErrorSource::Scanner, 0, "expected empty input",
                ));
            }
            assert_eq!(carry.prev_in_string, u64::MAX);
            Ok(BlockMasks::default())
        };
        let mut carry = ScanCarry { prev_in_string: u64::MAX, ..ScanCarry::default() };
        let before = carry;
        assert_eq!(classify(&[], &mut carry).unwrap(), BlockMasks::default());
        assert_eq!(carry, before);
        let error = classify(b"x", &mut carry).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidToken);
        assert_eq!(error.source, ErrorSource::Scanner);
    }
}
