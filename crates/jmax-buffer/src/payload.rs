//! Length-checked adoption and construction of canonical ABI payloads.
//!
//! These functions establish ownership and payload length limits only. Arbitrary
//! bytes, including invalid UTF-8 and invalid JSON, are accepted within the limit.
//! Allocation follows the infallible ABI constructors' normal allocator behavior.

use super::{Arc, BufferLimits, Error, ErrorCode, ErrorSource, Payload, Result, Vec};

/// Adopts an existing payload without copying or changing its visible range.
///
/// Both shared slices and the ABI's Bytes variant retain their storage variant.
/// Only visible length is checked; a sliced payload may retain a larger backing
/// allocation. This function neither validates contents nor issues a proof.
pub fn adopt(payload: Payload, limits: BufferLimits) -> Result<Payload> {
    validate_len(payload.len(), limits)?;
    Ok(payload)
}

/// Adopts a shared byte slice without copying its byte allocation.
pub fn from_arc(bytes: Arc<[u8]>, limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::Shared(bytes))
}

/// Transfers a vector through the canonical ABI ownership constructor.
///
/// Spare capacity does not count toward the payload limit. No capacity
/// preservation guarantee is made by this interface.
pub fn from_vec(bytes: Vec<u8>, limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::from_vec(bytes))
}

/// Copies borrowed bytes into a shared payload after checking their length.
pub fn from_slice(bytes: &[u8], limits: BufferLimits) -> Result<Payload> {
    validate_len(bytes.len(), limits)?;
    Ok(Payload::from_slice(bytes))
}

/// Checks both ABI representability and the configured payload limit.
///
/// Equality is allowed, including an empty payload with a zero limit. The
/// decoded arena limit is independent and does not affect payload admission.
/// Errors contain static, payload-free metadata with an unknown byte offset.
pub fn validate_len(len: usize, limits: BufferLimits) -> Result<u32> {
    let too_large = || {
        Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            u32::MAX,
            "payload length exceeds configured or ABI limit",
        )
    };
    let len = u32::try_from(len).map_err(|_| too_large())?;
    if len > limits.max_payload_bytes {
        return Err(too_large());
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_MAX_PAYLOAD_BYTES, MAX_BUFFER_BYTES};

    fn limits(max_payload_bytes: u32) -> BufferLimits {
        BufferLimits {
            max_payload_bytes,
            max_decoded_bytes: 0,
        }
    }

    fn assert_too_large(error: Error) {
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, u32::MAX);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert_eq!(
            error.detail,
            "payload length exceeds configured or ABI limit"
        );
    }

    // Obtain Bytes through the canonical ABI without importing its dependency.
    fn sliced_payload(range: core::ops::Range<usize>) -> Payload {
        match Payload::from_vec(b"prefix:body:suffix".to_vec()) {
            Payload::Bytes(bytes) => Payload::Bytes(bytes.slice(range)),
            Payload::Shared(_) => panic!("ABI vector constructor must produce Bytes"),
        }
    }

    #[test]
    fn length_validation_accepts_zero_and_exact_limits() {
        for max in [0, 1, 4, DEFAULT_MAX_PAYLOAD_BYTES, u32::MAX] {
            let configured = limits(max);
            assert_eq!(validate_len(0, configured), Ok(0));
            assert_eq!(validate_len(max as usize, configured), Ok(max));
            if max > 0 {
                assert_eq!(validate_len((max - 1) as usize, configured), Ok(max - 1));
            }
        }
        assert_eq!(validate_len(MAX_BUFFER_BYTES, limits(u32::MAX)), Ok(u32::MAX));
    }

    #[test]
    fn length_validation_rejects_one_byte_over_each_limit() {
        for max in [0, 1, 4, DEFAULT_MAX_PAYLOAD_BYTES, u32::MAX - 1] {
            assert_too_large(validate_len((max + 1) as usize, limits(max)).unwrap_err());
        }
    }

    #[test]
    fn length_validation_rejects_unrepresentable_lengths_without_allocating() {
        if let Some(over_abi) = MAX_BUFFER_BYTES.checked_add(1) {
            assert_too_large(validate_len(over_abi, limits(u32::MAX)).unwrap_err());
            assert_too_large(validate_len(over_abi, limits(0)).unwrap_err());
            assert_too_large(validate_len(usize::MAX, limits(u32::MAX)).unwrap_err());
        } else {
            // On a 32-bit target every usize fits the ABI, including usize::MAX.
            assert_eq!(validate_len(usize::MAX, limits(u32::MAX)), Ok(u32::MAX));
            assert_too_large(validate_len(usize::MAX, limits(u32::MAX - 1)).unwrap_err());
        }
    }

    #[test]
    fn default_limit_has_an_inclusive_boundary() {
        let configured = BufferLimits::default();
        assert_eq!(
            validate_len(DEFAULT_MAX_PAYLOAD_BYTES as usize, configured),
            Ok(DEFAULT_MAX_PAYLOAD_BYTES)
        );
        assert_too_large(
            validate_len(DEFAULT_MAX_PAYLOAD_BYTES as usize + 1, configured).unwrap_err()
        );
    }

    #[test]
    fn decoded_limit_is_independent_of_payload_limit() {
        for max_decoded_bytes in [0, 1, u32::MAX] {
            let configured = BufferLimits { max_payload_bytes: 4, max_decoded_bytes };
            assert_eq!(validate_len(4, configured), Ok(4));
            assert_too_large(validate_len(5, configured).unwrap_err());
            assert_eq!(from_slice(b"data", configured).unwrap().as_slice(), b"data");
        }
    }

    #[test]
    fn arc_conversion_preserves_allocation_and_shared_variant() {
        let original: Arc<[u8]> = Arc::from(&b"data"[..]);
        let payload = from_arc(Arc::clone(&original), limits(4)).unwrap();
        match &payload {
            Payload::Shared(bytes) => {
                assert!(Arc::ptr_eq(bytes, &original));
                assert_eq!(Arc::strong_count(bytes), 2);
            }
            Payload::Bytes(_) => panic!("Arc conversion changed storage variant"),
        }
        assert_eq!(payload.as_slice(), b"data");
        drop(original);
        assert_eq!(payload.as_slice(), b"data");
    }

    #[test]
    fn shared_adoption_preserves_allocation_without_cloning() {
        let original: Arc<[u8]> = Arc::from(&b"\0\xff\x80"[..]);
        let payload = Payload::Shared(Arc::clone(&original));
        let adopted = adopt(payload, limits(3)).unwrap();
        match &adopted {
            Payload::Shared(bytes) => {
                assert!(Arc::ptr_eq(bytes, &original));
                assert_eq!(Arc::strong_count(bytes), 2);
            }
            Payload::Bytes(_) => panic!("adoption changed shared storage variant"),
        }
        assert_eq!(adopted.as_slice(), &[0, 255, 128]);
    }

    #[test]
    fn bytes_adoption_preserves_sliced_visible_range_and_pointer() {
        let payload = sliced_payload(7..11);
        let observer = payload.clone();
        let pointer = payload.as_slice().as_ptr();
        let adopted = adopt(payload, limits(4)).unwrap();
        assert!(matches!(&adopted, Payload::Bytes(_)));
        assert_eq!(adopted.len(), 4);
        assert_eq!(adopted.as_slice(), b"body");
        assert_eq!(adopted.as_slice().as_ptr(), pointer);
        drop(observer);
        assert_eq!(adopted.as_slice(), b"body");
    }

    #[test]
    fn vector_conversion_transfers_visible_bytes_and_ignores_capacity() {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(b"body");
        let pointer = bytes.as_ptr();
        let payload = from_vec(bytes, limits(4)).unwrap();
        assert!(matches!(&payload, Payload::Bytes(_)));
        assert_eq!(payload.len(), 4);
        assert_eq!(payload.as_slice(), b"body");
        assert_eq!(payload.as_slice().as_ptr(), pointer);
        let clone = payload.clone();
        drop(payload);
        assert_eq!(clone.as_slice(), b"body");
    }

    #[test]
    fn slice_conversion_copies_into_independent_shared_storage() {
        let mut bytes = [0, 255, 128, b'{'];
        let payload = from_slice(&bytes, limits(4)).unwrap();
        assert!(matches!(&payload, Payload::Shared(_)));
        assert_ne!(payload.as_slice().as_ptr(), bytes.as_ptr());
        bytes.fill(7);
        assert_eq!(payload.as_slice(), &[0, 255, 128, b'{']);
        assert_eq!(bytes, [7; 4]);
    }

    #[test]
    fn every_entry_point_accepts_empty_payloads_at_zero_limit() {
        let configured = limits(0);
        let mut spare_capacity = Vec::with_capacity(32);
        spare_capacity.extend_from_slice(b"discarded");
        spare_capacity.clear();
        let empty_shared: Arc<[u8]> = Arc::from(&b""[..]);
        let payloads = [
            adopt(Payload::Shared(Arc::clone(&empty_shared)), configured).unwrap(),
            adopt(sliced_payload(7..7), configured).unwrap(),
            from_arc(empty_shared, configured).unwrap(),
            from_vec(Vec::new(), configured).unwrap(),
            from_vec(spare_capacity, configured).unwrap(),
            from_slice(&[], configured).unwrap(),
        ];
        for payload in payloads {
            assert!(payload.is_empty());
            assert_eq!(payload.len(), 0);
            assert_eq!(payload.as_slice(), &[]);
        }
    }

    #[test]
    fn every_entry_point_rejects_nonempty_payloads_at_zero_limit() {
        let configured = limits(0);
        assert_too_large(adopt(Payload::from_slice(b"x"), configured).unwrap_err());
        assert_too_large(adopt(sliced_payload(7..8), configured).unwrap_err());
        assert_too_large(from_arc(Arc::from(&b"x"[..]), configured).unwrap_err());
        assert_too_large(from_vec(b"x".to_vec(), configured).unwrap_err());
        assert_too_large(from_slice(b"x", configured).unwrap_err());
    }

    #[test]
    fn every_entry_point_reports_identical_over_limit_errors() {
        let configured = limits(3);
        let expected = validate_len(4, configured).unwrap_err();
        let errors = [
            adopt(Payload::from_slice(b"body"), configured).unwrap_err(),
            adopt(sliced_payload(7..11), configured).unwrap_err(),
            from_arc(Arc::from(&b"body"[..]), configured).unwrap_err(),
            from_vec(b"body".to_vec(), configured).unwrap_err(),
            from_slice(b"body", configured).unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error, expected);
            assert_too_large(error);
        }
    }

    #[test]
    fn rejection_releases_consumed_shared_owner_without_changing_other_owners() {
        let original: Arc<[u8]> = Arc::from(&b"body"[..]);
        assert_too_large(from_arc(Arc::clone(&original), limits(3)).unwrap_err());
        assert_eq!(Arc::strong_count(&original), 1);
        assert_too_large(
            adopt(Payload::Shared(Arc::clone(&original)), limits(3)).unwrap_err()
        );
        assert_eq!(Arc::strong_count(&original), 1);
        assert_eq!(original.as_ref(), b"body");
    }

    #[test]
    fn arbitrary_binary_and_malformed_json_are_accepted_without_validation() {
        let vectors: &[&[u8]] = &[
            b"\xff\x80\0",
            b"{",
            b"[1,]",
            b"\"\\uD800\"",
            b"null trailing",
        ];
        for &bytes in vectors {
            let configured = limits(u32::try_from(bytes.len()).unwrap());
            let payloads = [
                from_slice(bytes, configured).unwrap(),
                from_vec(bytes.to_vec(), configured).unwrap(),
                from_arc(Arc::from(bytes), configured).unwrap(),
                adopt(Payload::from_slice(bytes), configured).unwrap(),
                adopt(Payload::from_vec(bytes.to_vec()), configured).unwrap(),
            ];
            for payload in payloads {
                assert_eq!(payload.as_slice(), bytes);
                assert_eq!(payload.len(), bytes.len());
            }
        }
    }
}
