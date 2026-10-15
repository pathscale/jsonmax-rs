//! Fallible byte output with inclusive limits and checked sink delegation.

use super::*;

/// Assures capacity without changing the sink's logical output length.
///
/// `limit` includes the sink's current length. Reservations do not consume any
/// of that allowance; every later write must independently respect its limit.
pub fn reserve(sink: &mut dyn ByteSink, additional: usize, limit: usize) -> Result<()> {
    checked_output_len(sink.len(), additional, limit)?;
    sink.reserve(additional)
}

/// Reserves capacity and then appends exactly `bytes` within an inclusive limit.
///
/// An external sink error is preserved, and a failed write may leave a prefix
/// in that sink. This function cannot roll back an arbitrary destination.
pub fn append(sink: &mut dyn ByteSink, bytes: &[u8], limit: usize) -> Result<()> {
    reserve(sink, bytes.len(), limit)?;
    sink.write_bytes(bytes)
}

impl ByteSink for VecSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        checked_output_len(self.bytes.len(), additional, self.limit)?;
        self.bytes.try_reserve(additional).map_err(|_| {
            encode_error(ErrorCode::BudgetExhausted, "output allocation failed")
        })
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.reserve(bytes.len())?;
        // The fallible reservation above guarantees capacity for this append.
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl ByteSink for LimitedSink<'_> {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if self.poisoned {
            return Err(encode_error(ErrorCode::InternalError, "output sink is poisoned"));
        }
        let before = self.inner.len();
        let Some(appended) = before.checked_sub(self.start_len) else {
            self.poisoned = true;
            return Err(encode_error(ErrorCode::InternalError, "inner sink removed its prefix"));
        };
        checked_output_len(appended, additional, self.limit)?;
        // A large existing prefix must not hide overflow in the inner length.
        checked_output_len(before, additional, usize::MAX)?;
        if let Err(error) = self.inner.reserve(additional) {
            self.poisoned = true;
            return Err(error);
        }
        if self.inner.len() != before {
            self.poisoned = true;
            return Err(encode_error(ErrorCode::InternalError, "reservation changed inner length"));
        }
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.poisoned {
            return Err(encode_error(ErrorCode::InternalError, "output sink is poisoned"));
        }
        let before = self.inner.len();
        let Some(appended) = before.checked_sub(self.start_len) else {
            self.poisoned = true;
            return Err(encode_error(ErrorCode::InternalError, "inner sink removed its prefix"));
        };
        checked_output_len(appended, bytes.len(), self.limit)?;
        let expected = checked_output_len(before, bytes.len(), usize::MAX)?;
        if let Err(error) = self.inner.write_bytes(bytes) {
            self.poisoned = true;
            return Err(error);
        }
        if self.inner.len() != expected {
            self.poisoned = true;
            return Err(encode_error(ErrorCode::InternalError, "write changed inner length incorrectly"));
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.appended_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_encode_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Encode);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn external_error() -> Error {
        Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 17, "sink failure")
            .with_schema(23, 29)
    }

    /// Records actual output and rejects calls in the selected phase.
    struct RecordingSink {
        output: VecSink,
        reservations: usize,
        writes: usize,
        last_reservation: usize,
        reject_reserve: bool,
        reject_write: bool,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                output: VecSink::new(64),
                reservations: 0,
                writes: 0,
                last_reservation: 0,
                reject_reserve: false,
                reject_write: false,
            }
        }
    }

    impl ByteSink for RecordingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.reservations += 1;
            self.last_reservation = additional;
            if self.reject_reserve {
                return Err(external_error());
            }
            self.output.reserve(additional)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            if self.reject_write {
                return Err(external_error());
            }
            self.output.write_bytes(bytes)
        }

        fn len(&self) -> usize {
            self.output.len()
        }
    }

    /// Deliberately violates length contracts without huge backing allocations.
    #[derive(Clone, Copy)]
    enum Effect {
        Normal,
        Grow(usize),
        Shrink(usize),
        FailAfter(usize),
    }

    struct ContractSink {
        length: usize,
        reserve_effect: Effect,
        write_effect: Effect,
        reservations: usize,
        writes: usize,
    }

    impl ContractSink {
        fn new(length: usize, reserve_effect: Effect, write_effect: Effect) -> Self {
            Self { length, reserve_effect, write_effect, reservations: 0, writes: 0 }
        }

        fn apply(&mut self, effect: Effect, normal_growth: usize) -> Result<()> {
            match effect {
                Effect::Normal => {
                    self.length = checked_output_len(self.length, normal_growth, usize::MAX)?;
                    Ok(())
                }
                Effect::Grow(count) => {
                    self.length = checked_output_len(self.length, count, usize::MAX)?;
                    Ok(())
                }
                Effect::Shrink(count) => {
                    self.length = self.length.checked_sub(count).expect("test shrink is in range");
                    Ok(())
                }
                Effect::FailAfter(count) => {
                    self.length = checked_output_len(self.length, count, usize::MAX)?;
                    Err(external_error())
                }
            }
        }
    }

    impl ByteSink for ContractSink {
        fn reserve(&mut self, _additional: usize) -> Result<()> {
            self.reservations += 1;
            self.apply(self.reserve_effect, 0)
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes += 1;
            self.apply(self.write_effect, bytes.len())
        }

        fn len(&self) -> usize {
            self.length
        }
    }

    #[test]
    fn free_reserve_only_assures_capacity_and_rechecks_current_length() {
        let mut sink = RecordingSink::new();
        sink.write_bytes(b"ab").unwrap();
        reserve(&mut sink, 3, 5).unwrap();
        reserve(&mut sink, 3, 5).unwrap();
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.last_reservation, 3);
        assert_eq!(sink.writes, 1);
        assert_eq!(sink.output.as_slice(), b"ab");
        assert!(sink.output.bytes.capacity() >= 5);
        assert_encode_error(reserve(&mut sink, 4, 5).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_encode_error(reserve(&mut sink, 0, 1).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.output.as_slice(), b"ab");
    }

    #[test]
    fn free_append_writes_exact_binary_slice_once_after_reserving() {
        let mut sink = RecordingSink::new();
        append(&mut sink, &[0, 0xff, b'"', b'\\'], 4).unwrap();
        assert_eq!(sink.output.as_slice(), &[0, 0xff, b'"', b'\\']);
        assert_eq!(sink.reservations, 1);
        assert_eq!(sink.last_reservation, 4);
        assert_eq!(sink.writes, 1);
        append(&mut sink, b"", 4).unwrap();
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.last_reservation, 0);
        assert_eq!(sink.writes, 2);
        assert_encode_error(append(&mut sink, b"x", 4).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.writes, 2);
        assert_eq!(sink.output.len(), 4);
    }

    #[test]
    fn free_helpers_reject_overflow_before_any_delegation() {
        let mut sink = ContractSink::new(usize::MAX, Effect::Normal, Effect::Normal);
        assert_encode_error(reserve(&mut sink, 1, usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_encode_error(append(&mut sink, b"x", usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.reservations, 0);
        assert_eq!(sink.writes, 0);
        reserve(&mut sink, 0, usize::MAX).unwrap();
        append(&mut sink, b"", usize::MAX).unwrap();
        assert_eq!(sink.len(), usize::MAX);
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.writes, 1);
    }

    #[test]
    fn free_helpers_preserve_errors_and_stop_after_reservation_failure() {
        let mut sink = RecordingSink::new();
        sink.reject_reserve = true;
        assert_eq!(reserve(&mut sink, 1, 8).unwrap_err(), external_error());
        assert_eq!(append(&mut sink, b"a", 8).unwrap_err(), external_error());
        assert_eq!(sink.reservations, 2);
        assert_eq!(sink.writes, 0);
        sink.reject_reserve = false;
        sink.reject_write = true;
        assert_eq!(append(&mut sink, b"b", 8).unwrap_err(), external_error());
        assert_eq!(sink.reservations, 3);
        assert_eq!(sink.writes, 1);
        assert!(sink.output.is_empty());
    }

    #[test]
    fn free_append_preserves_a_partial_external_write() {
        let mut sink = ContractSink::new(7, Effect::Normal, Effect::FailAfter(2));
        assert_eq!(append(&mut sink, b"abcd", 11).unwrap_err(), external_error());
        assert_eq!(sink.len(), 9);
        assert_eq!(sink.reservations, 1);
        assert_eq!(sink.writes, 1);
    }

    #[test]
    fn zero_limits_accept_only_empty_output() {
        let mut sink = VecSink::new(0);
        assert_eq!(sink.limit(), 0);
        assert!(sink.is_empty());
        reserve(&mut sink, 0, 0).unwrap();
        append(&mut sink, b"", 0).unwrap();
        sink.reserve(0).unwrap();
        sink.write_bytes(b"").unwrap();
        assert_encode_error(sink.reserve(1).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_encode_error(sink.write_byte(0).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert!(sink.is_empty());

        let mut inner = VecSink::new(8);
        inner.write_bytes(b"prefix").unwrap();
        {
            let mut limited = LimitedSink::new(&mut inner, 0);
            limited.reserve(0).unwrap();
            limited.write_bytes(b"").unwrap();
            append(&mut limited, b"", 0).unwrap();
            assert_encode_error(limited.reserve(1).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_encode_error(limited.write_byte(b'x').unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_eq!(limited.appended_len(), 0);
            assert!(limited.is_empty());
            assert!(!limited.is_poisoned());
        }
        assert_eq!(inner.as_slice(), b"prefix");
    }

    #[test]
    fn vector_reservations_do_not_charge_subsequent_writes() {
        let mut sink = VecSink::new(5);
        sink.reserve(5).unwrap();
        sink.reserve(5).unwrap();
        assert!(sink.is_empty());
        assert!(sink.bytes.capacity() >= 5);
        sink.write_bytes(b"ab").unwrap();
        sink.reserve(3).unwrap();
        sink.write_bytes(b"cd").unwrap();
        sink.write_byte(b'e').unwrap();
        sink.reserve(0).unwrap();
        sink.write_bytes(b"").unwrap();
        assert_eq!(sink.len(), 5);
        assert_eq!(sink.as_slice(), b"abcde");
        assert_encode_error(sink.reserve(1).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_encode_error(sink.write_bytes(b"f").unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.as_slice(), b"abcde");
    }

    #[test]
    fn vector_rejected_write_is_atomic_and_does_not_poison() {
        let mut sink = VecSink::new(4);
        sink.write_bytes(b"ab").unwrap();
        let capacity = sink.bytes.capacity();
        assert_encode_error(sink.write_bytes(b"cde").unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.as_slice(), b"ab");
        assert_eq!(sink.bytes.capacity(), capacity);
        sink.write_bytes(b"cd").unwrap();
        assert_eq!(sink.as_slice(), b"abcd");
        assert_encode_error(sink.reserve(usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
        assert_eq!(sink.as_slice(), b"abcd");
    }

    #[test]
    fn vector_capacity_overflow_is_deterministic_and_preserves_storage() {
        let mut sink = VecSink::new(usize::MAX);
        let capacity_overflow = (isize::MAX as usize).checked_add(1).unwrap();
        assert_encode_error(sink.reserve(capacity_overflow).unwrap_err(), ErrorCode::BudgetExhausted);
        assert!(sink.is_empty());
        assert_eq!(sink.bytes.capacity(), 0);
        sink.write_byte(b'x').unwrap();
        let capacity = sink.bytes.capacity();
        assert_encode_error(sink.reserve(capacity_overflow).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(sink.as_slice(), b"x");
        assert_eq!(sink.bytes.capacity(), capacity);
        sink.write_byte(b'y').unwrap();
        assert_eq!(sink.as_slice(), b"xy");
    }

    #[test]
    fn into_vec_transfers_the_original_allocation() {
        let mut sink = VecSink::new(16);
        sink.reserve(16).unwrap();
        sink.write_bytes(b"payload").unwrap();
        let pointer = sink.bytes.as_ptr();
        let capacity = sink.bytes.capacity();
        let bytes = sink.into_vec();
        assert_eq!(bytes.as_slice(), b"payload");
        assert_eq!(bytes.as_ptr(), pointer);
        assert_eq!(bytes.capacity(), capacity);
    }

    #[test]
    fn limited_output_counts_only_new_bytes_and_preserves_prefix() {
        let mut inner = VecSink::new(16);
        inner.write_bytes(b"prefix:").unwrap();
        {
            let mut sink = LimitedSink::new(&mut inner, 3);
            assert!(sink.is_empty());
            sink.reserve(3).unwrap();
            sink.reserve(3).unwrap();
            assert_eq!(sink.len(), 0);
            sink.write_bytes(b"ab").unwrap();
            assert_eq!(sink.len(), 2);
            assert_eq!(sink.appended_len(), 2);
            assert!(!sink.is_empty());
            sink.write_byte(b'c').unwrap();
            sink.reserve(0).unwrap();
            sink.write_bytes(b"").unwrap();
            assert_eq!(sink.len(), 3);
            assert_encode_error(sink.reserve(1).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_encode_error(sink.write_bytes(b"d").unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_eq!(sink.len(), 3);
            assert!(!sink.is_poisoned());
        }
        assert_eq!(inner.as_slice(), b"prefix:abc");
    }

    #[test]
    fn limited_limit_rejection_does_not_delegate_or_prevent_smaller_writes() {
        let mut inner = RecordingSink::new();
        {
            let mut sink = LimitedSink::new(&mut inner, 2);
            assert_encode_error(sink.reserve(3).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_encode_error(sink.write_bytes(b"abc").unwrap_err(), ErrorCode::PayloadTooLarge);
            assert!(!sink.is_poisoned());
            sink.write_byte(b'a').unwrap();
            assert_encode_error(sink.reserve(usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
            sink.write_byte(b'b').unwrap();
            assert_eq!(sink.len(), 2);
        }
        assert_eq!(inner.reservations, 0);
        assert_eq!(inner.writes, 2);
        assert_eq!(inner.output.as_slice(), b"ab");
    }

    #[test]
    fn free_helpers_use_the_limited_sinks_relative_length() {
        let mut inner = VecSink::new(32);
        inner.write_bytes(b"long existing prefix:").unwrap();
        {
            let mut sink = LimitedSink::new(&mut inner, 5);
            reserve(&mut sink, 3, 3).unwrap();
            append(&mut sink, b"abc", 3).unwrap();
            assert_encode_error(append(&mut sink, b"d", 3).unwrap_err(), ErrorCode::PayloadTooLarge);
            append(&mut sink, b"de", 5).unwrap();
            assert_eq!(sink.len(), 5);
            assert!(!sink.is_poisoned());
        }
        assert_eq!(inner.as_slice(), b"long existing prefix:abcde");
    }

    #[test]
    fn nested_limits_count_output_since_each_wrapper_was_created() {
        let mut inner = VecSink::new(32);
        inner.write_bytes(b"prefix").unwrap();
        {
            let mut outer = LimitedSink::new(&mut inner, 5);
            outer.write_bytes(b"a").unwrap();
            {
                let mut nested = LimitedSink::new(&mut outer, 3);
                nested.reserve(3).unwrap();
                nested.write_bytes(b"bcd").unwrap();
                assert_eq!(nested.len(), 3);
                assert_encode_error(nested.write_byte(b'e').unwrap_err(), ErrorCode::PayloadTooLarge);
                assert!(!nested.is_poisoned());
            }
            assert_eq!(outer.len(), 4);
            outer.write_byte(b'e').unwrap();
        }
        assert_eq!(inner.as_slice(), b"prefixabcde");
    }

    #[test]
    fn limited_inner_errors_preserve_metadata_and_poison_both_operations() {
        for failure_in_reserve in [false, true] {
            let mut inner = RecordingSink::new();
            inner.reject_reserve = failure_in_reserve;
            inner.reject_write = !failure_in_reserve;
            {
                let mut sink = LimitedSink::new(&mut inner, 8);
                let error = if failure_in_reserve {
                    sink.reserve(1).unwrap_err()
                } else {
                    sink.write_byte(b'a').unwrap_err()
                };
                assert_eq!(error, external_error());
                assert!(sink.is_poisoned());
                assert_encode_error(sink.reserve(0).unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.reserve(usize::MAX).unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.write_bytes(b"").unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.write_byte(b'x').unwrap_err(), ErrorCode::InternalError);
                assert_eq!(sink.len(), 0);
            }
            assert_eq!(inner.reservations, usize::from(failure_in_reserve));
            assert_eq!(inner.writes, usize::from(!failure_in_reserve));
        }
    }

    #[test]
    fn inner_limit_errors_poison_a_wrapper_even_when_the_write_was_atomic() {
        let mut inner = VecSink::new(2);
        inner.write_byte(b'a').unwrap();
        {
            let mut sink = LimitedSink::new(&mut inner, 8);
            assert_encode_error(sink.write_bytes(b"bc").unwrap_err(), ErrorCode::PayloadTooLarge);
            assert!(sink.is_poisoned());
            assert_eq!(sink.len(), 0);
            assert_encode_error(sink.write_byte(b'b').unwrap_err(), ErrorCode::InternalError);
        }
        assert_eq!(inner.as_slice(), b"a");
    }

    #[test]
    fn partial_inner_errors_are_preserved_without_fabricating_rollback() {
        for failure_in_reserve in [false, true] {
            let mut inner = ContractSink::new(
                5,
                if failure_in_reserve { Effect::FailAfter(1) } else { Effect::Normal },
                if failure_in_reserve { Effect::Normal } else { Effect::FailAfter(2) },
            );
            {
                let mut sink = LimitedSink::new(&mut inner, 3);
                let error = if failure_in_reserve {
                    sink.reserve(3).unwrap_err()
                } else {
                    sink.write_bytes(b"abc").unwrap_err()
                };
                assert_eq!(error, external_error());
                assert!(sink.is_poisoned());
                assert_eq!(sink.len(), if failure_in_reserve { 1 } else { 2 });
                assert_encode_error(sink.reserve(0).unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.write_bytes(b"").unwrap_err(), ErrorCode::InternalError);
            }
            assert_eq!(inner.len(), if failure_in_reserve { 6 } else { 7 });
            assert_eq!(inner.reservations, usize::from(failure_in_reserve));
            assert_eq!(inner.writes, usize::from(!failure_in_reserve));
        }
    }

    #[test]
    fn successful_reservations_must_not_change_inner_length() {
        for effect in [Effect::Grow(1), Effect::Shrink(1)] {
            let mut inner = ContractSink::new(4, effect, Effect::Normal);
            {
                let mut sink = LimitedSink::new(&mut inner, 4);
                assert_encode_error(sink.reserve(2).unwrap_err(), ErrorCode::InternalError);
                assert!(sink.is_poisoned());
                assert_encode_error(sink.reserve(0).unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.write_byte(b'x').unwrap_err(), ErrorCode::InternalError);
            }
            assert_eq!(inner.reservations, 1);
            assert_eq!(inner.writes, 0);
        }
    }

    #[test]
    fn successful_writes_must_append_exactly_the_requested_length() {
        for effect in [Effect::Grow(0), Effect::Grow(1), Effect::Grow(3), Effect::Shrink(1)] {
            let mut inner = ContractSink::new(4, Effect::Normal, effect);
            {
                let mut sink = LimitedSink::new(&mut inner, 2);
                assert_encode_error(sink.write_bytes(b"ab").unwrap_err(), ErrorCode::InternalError);
                assert!(sink.is_poisoned());
                assert_encode_error(sink.reserve(0).unwrap_err(), ErrorCode::InternalError);
                assert_encode_error(sink.write_bytes(b"").unwrap_err(), ErrorCode::InternalError);
            }
            assert_eq!(inner.writes, 1);
            assert_eq!(inner.reservations, 0);
        }
    }

    #[test]
    fn even_empty_calls_enforce_inner_length_contracts() {
        for reserve_call in [false, true] {
            let mut inner = ContractSink::new(0, Effect::Grow(1), Effect::Grow(1));
            let mut sink = LimitedSink::new(&mut inner, 0);
            let error = if reserve_call {
                sink.reserve(0).unwrap_err()
            } else {
                sink.write_bytes(b"").unwrap_err()
            };
            assert_encode_error(error, ErrorCode::InternalError);
            assert!(sink.is_poisoned());
            assert_eq!(sink.len(), 1);
        }
    }

    #[test]
    fn missing_existing_prefix_is_detected_before_delegation() {
        struct MutableLengthSink<'a> {
            length: &'a core::cell::Cell<usize>,
            delegations: usize,
        }

        impl ByteSink for MutableLengthSink<'_> {
            fn reserve(&mut self, additional: usize) -> Result<()> {
                self.delegations += 1;
                checked_output_len(self.length.get(), additional, usize::MAX)?;
                Ok(())
            }

            fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
                self.delegations += 1;
                let next = checked_output_len(self.length.get(), bytes.len(), usize::MAX)?;
                self.length.set(next);
                Ok(())
            }

            fn len(&self) -> usize {
                self.length.get()
            }
        }

        for reserve_call in [false, true] {
            let length = core::cell::Cell::new(3);
            let mut inner = MutableLengthSink { length: &length, delegations: 0 };
            {
                let mut sink = LimitedSink::new(&mut inner, 4);
                // Interior mutation can violate a borrowed sink's append-only
                // contract without mutable reference aliasing or unsafe code.
                length.set(2);
                assert_eq!(sink.len(), 0);
                let error = if reserve_call {
                    sink.reserve(0).unwrap_err()
                } else {
                    sink.write_bytes(b"").unwrap_err()
                };
                assert_encode_error(error, ErrorCode::InternalError);
                assert!(sink.is_poisoned());
            }
            assert_eq!(inner.delegations, 0);
        }
    }

    #[test]
    fn limited_arithmetic_checks_both_relative_and_absolute_lengths() {
        let mut inner = ContractSink::new(usize::MAX - 1, Effect::Normal, Effect::Normal);
        {
            let mut sink = LimitedSink::new(&mut inner, usize::MAX);
            assert_encode_error(sink.reserve(2).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_encode_error(sink.write_bytes(b"ab").unwrap_err(), ErrorCode::PayloadTooLarge);
            assert!(!sink.is_poisoned());
            sink.reserve(1).unwrap();
            sink.write_byte(b'a').unwrap();
            assert_eq!(sink.len(), 1);
            assert_encode_error(sink.reserve(usize::MAX).unwrap_err(), ErrorCode::PayloadTooLarge);
            assert_encode_error(sink.write_byte(b'b').unwrap_err(), ErrorCode::PayloadTooLarge);
            sink.reserve(0).unwrap();
            sink.write_bytes(b"").unwrap();
            assert_eq!(sink.len(), 1);
            assert!(!sink.is_poisoned());
        }
        assert_eq!(inner.len(), usize::MAX);
        assert_eq!(inner.reservations, 2);
        assert_eq!(inner.writes, 2);
    }
}
