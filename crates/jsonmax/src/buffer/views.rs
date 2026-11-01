//! Checked borrows of raw payload and decoded arena storage.
//!
//! A reference supplies coordinates and a source, but no owner identity. The
//! caller selects the owner against which it is resolved. These views check
//! storage bounds and string metadata without authenticating provenance,
//! interpreting JSON escapes, or issuing validation proofs.

use super::{
    checked_range, checked_span, ByteView, Error, ErrorCode, ErrorSource,
    OwnerBytes, ParsedMessage, Result, Span32, SpanSource, StrRef, StrView,
};

impl<'a> OwnerBytes<'a> {
    /// Borrows both buffers after checking their lengths fit the ABI.
    ///
    /// Empty buffers are valid. No configured admission limit or content
    /// validation is applied; each buffer may contain arbitrary bytes.
    pub fn new(raw: &'a [u8], decoded: &'a [u8]) -> Result<Self> {
        validate_owner_lengths(raw.len(), decoded.len())?;
        Ok(Self { raw, decoded })
    }

    /// Borrows the message's actual buffers without consulting its proof.
    ///
    /// Subsequent string lookups still validate UTF-8 and scalar counts, even
    /// when the reference did not originate in the message's validated values.
    pub fn from_message<T>(message: &'a ParsedMessage<T>) -> Result<Self> {
        Self::new(message.raw_bytes(), message.decoded_bytes())
    }

    /// Returns the entire raw payload borrowed from the owner.
    pub fn raw_bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// Returns all logical decoded bytes borrowed from the owner.
    pub fn decoded_bytes(&self) -> &'a [u8] {
        self.decoded
    }

    /// Checks a span against exactly the selected buffer and borrows its bytes.
    ///
    /// Empty spans at the buffer end are valid. Byte views do not require UTF-8
    /// boundaries. The returned borrow can outlive this wrapper, but cannot
    /// outlive the underlying storage.
    pub fn view(&self, source: SpanSource, span: Span32) -> Result<ByteView<'a>> {
        let buffer = match source {
            SpanSource::RawPayload => self.raw,
            SpanSource::DecodedArena => self.decoded,
        };
        let range = checked_range(span, buffer.len())?;
        Ok(ByteView { bytes: &buffer[range], span, source })
    }

    /// Resolves a string after checking bounds, UTF-8, and Unicode scalar count.
    ///
    /// Length metadata counts Unicode scalars, while the span and returned
    /// view's length count UTF-8 bytes. Validation examines only this span, so
    /// unrelated bytes in either buffer need not be valid UTF-8 or JSON.
    pub fn string(&self, reference: StrRef) -> Result<StrView<'a>> {
        let bytes = self.view(reference.source, reference.span)?;
        let text = match core::str::from_utf8(bytes.as_bytes()) {
            Ok(text) => text,
            Err(error) => {
                // The checked view bounds valid_up_to by span.len, and the
                // span's end fits u32. Keep the conversion and addition checked
                // as well, so diagnostic construction never truncates or wraps.
                let invalid_span = || Error::new(
                    ErrorCode::InvalidToken,
                    ErrorSource::Decode,
                    reference.span.offset,
                    "buffer span is out of bounds",
                );
                let valid_up_to = u32::try_from(error.valid_up_to())
                    .map_err(|_| invalid_span())?;
                let byte_offset = reference.span.offset.checked_add(valid_up_to)
                    .ok_or_else(invalid_span)?;
                return Err(Error::new(
                    ErrorCode::InvalidUtf8,
                    ErrorSource::Decode,
                    byte_offset,
                    "buffer string is not valid UTF-8",
                ));
            }
        };
        // A scalar occupies at least one byte, so a checked span also bounds
        // its count by u32::MAX. Compare without an unchecked conversion.
        if u32::try_from(text.chars().count()).ok() != Some(reference.scalar_len) {
            return Err(Error::new(
                ErrorCode::ConstraintViolation,
                ErrorSource::Decode,
                reference.span.offset,
                "buffer string scalar count does not match reference",
            ));
        }
        Ok(StrView { text, reference })
    }
}

// Separate length-only admission permits boundary tests without huge buffers or
// manufacturing invalid slices. Raw admission intentionally precedes decoded.
fn validate_owner_lengths(raw_len: usize, decoded_len: usize) -> Result<()> {
    checked_span(0, raw_len, SpanSource::RawPayload)?;
    checked_span(0, decoded_len, SpanSource::DecodedArena)?;
    Ok(())
}
