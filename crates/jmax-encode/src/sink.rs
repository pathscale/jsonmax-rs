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
        self.bytes
            .try_reserve(additional)
            .map_err(|_| encode_error(ErrorCode::BudgetExhausted, "output allocation failed"))
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let next = checked_output_len(self.bytes.len(), bytes.len(), self.limit)?;
        if next > self.bytes.capacity() {
            self.reserve(bytes.len())?;
        }
        // The fallible reservation above guarantees capacity for this append.
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl LimitedSink<'_> {
    /// Preallocate message storage without extending a token's pending suffix.
    /// Escape growth must apply to that token, not to the whole capacity hint.
    pub(crate) fn reserve_capacity(&mut self, additional: usize) -> Result<()> {
        let pending = self.reserved_len;
        self.reserve(additional)?;
        self.reserved_len = pending;
        Ok(())
    }
    /// Extend the pending reservation, including its unconsumed suffix. The
    /// ByteSink ABI reserves relative to current length, so delegation includes
    /// that suffix even though the output budget grows only by `additional`.
    pub(crate) fn reserve_extra(&mut self, additional: usize) -> Result<()> {
        let appended = self.appended_len();
        let end = checked_output_len(self.reserved_len.max(appended), additional, self.limit)?;
        self.reserve(end - appended)
    }
}

impl ByteSink for LimitedSink<'_> {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if self.poisoned {
            return Err(encode_error(
                ErrorCode::InternalError,
                "output sink is poisoned",
            ));
        }
        let before = self.inner.len();
        let Some(appended) = before.checked_sub(self.start_len) else {
            self.poisoned = true;
            return Err(encode_error(
                ErrorCode::InternalError,
                "inner sink removed its prefix",
            ));
        };
        let reserved = checked_output_len(appended, additional, self.limit)?;
        // A large existing prefix must not hide overflow in the inner length.
        checked_output_len(before, additional, usize::MAX)?;
        if reserved <= self.capacity_len {
            self.reserved_len = self.reserved_len.max(reserved);
            return Ok(());
        }
        if let Err(error) = self.inner.reserve(additional) {
            self.poisoned = true;
            return Err(error);
        }
        if self.inner.len() != before {
            self.poisoned = true;
            return Err(encode_error(
                ErrorCode::InternalError,
                "reservation changed inner length",
            ));
        }
        self.reserved_len = reserved;
        self.capacity_len = reserved;
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if self.poisoned {
            return Err(encode_error(
                ErrorCode::InternalError,
                "output sink is poisoned",
            ));
        }
        let before = self.inner.len();
        let Some(appended) = before.checked_sub(self.start_len) else {
            self.poisoned = true;
            return Err(encode_error(
                ErrorCode::InternalError,
                "inner sink removed its prefix",
            ));
        };
        checked_output_len(appended, bytes.len(), self.limit)?;
        let expected = checked_output_len(before, bytes.len(), usize::MAX)?;
        if let Err(error) = self.inner.write_bytes(bytes) {
            self.poisoned = true;
            return Err(error);
        }
        if self.inner.len() != expected {
            self.poisoned = true;
            return Err(encode_error(
                ErrorCode::InternalError,
                "write changed inner length incorrectly",
            ));
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.appended_len()
    }
}
