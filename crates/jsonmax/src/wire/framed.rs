//! Bounded, transport-neutral framing with a five-byte little-endian header.
//!
//! Payload bytes are opaque. The caller must keep the unconsumed suffix of each
//! `push`, drain the completed frame, then supply that suffix again.

#![forbid(unsafe_code)]

use super::*;

/// Decodes the first header, ignoring any bytes following it.
///
/// Configuration is validated before input. With valid limits, every prefix
/// shorter than [`FRAME_HEADER_BYTES`] returns `TransportIncomplete`.
pub fn decode_header(bytes: &[u8], limits: &FrameLimits) -> Result<FramedHeader> {
    validate_limits(limits)?;
    if bytes.len() < FRAME_HEADER_BYTES {
        return Err(wire_error(ErrorCode::TransportIncomplete, "incomplete frame header"));
    }
    let header = FramedHeader {
        payload_len: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        frame_kind: bytes[4],
    };
    validate_header(header, limits)?;
    Ok(header)
}

/// Writes the exact wire header and the original, unmodified payload.
///
/// Invalid configuration, metadata, or payload length causes no sink calls.
/// Sink errors propagate unchanged and may leave an output prefix.
pub fn encode_frame(frame: &Frame, limits: &FrameLimits, sink: &mut dyn ByteSink) -> Result<()> {
    validate_limits(limits)?;
    let total = validate_header(frame.header, limits)?;
    if frame.payload.len() != total - FRAME_HEADER_BYTES {
        return Err(wire_error(ErrorCode::FramingError, "frame payload length disagrees with header"));
    }
    let length = frame.header.payload_len.to_le_bytes();
    let header = [length[0], length[1], length[2], length[3], frame.header.frame_kind];
    sink.write_bytes(&header)?;
    sink.write_bytes(frame.payload.as_slice())
}

fn frame_size(payload_len: u32) -> Result<usize> {
    usize::try_from(payload_len).ok()
        .and_then(|length| length.checked_add(FRAME_HEADER_BYTES))
        .ok_or_else(|| wire_error(ErrorCode::FramingError, "frame length exceeds address space"))
}

fn validate_limits(limits: &FrameLimits) -> Result<()> {
    if limits.max_payload_bytes == 0 {
        return Err(wire_error(ErrorCode::FramingError, "frame payload limit must be nonzero"));
    }
    frame_size(limits.max_payload_bytes)?;
    // The kind domain is fixed at 256 values, independent of attacker input.
    let mut seen = [false; 256];
    for &kind in &limits.allowed_kinds {
        if core::mem::replace(&mut seen[usize::from(kind)], true) {
            return Err(wire_error(ErrorCode::FramingError, "duplicate allowed frame kind"));
        }
    }
    Ok(())
}

fn validate_header(header: FramedHeader, limits: &FrameLimits) -> Result<usize> {
    if header.payload_len == 0 {
        return Err(wire_error(ErrorCode::FramingError, "empty frame payload is forbidden"));
    }
    if header.payload_len > limits.max_payload_bytes {
        return Err(wire_error(ErrorCode::PayloadTooLarge, "frame payload exceeds limit"));
    }
    if !limits.allowed_kinds.contains(&header.frame_kind) {
        return Err(wire_error(ErrorCode::FramingError, "frame kind is not allowed"));
    }
    frame_size(header.payload_len)
}

impl FrameDecoder {
    /// Creates an empty decoder without allocating a payload-sized buffer.
    pub fn new(limits: FrameLimits) -> Result<Self> {
        validate_limits(&limits)?;
        Ok(Self { limits, buffer: Vec::new(), failed: false })
    }

    /// Copies only the prefix needed to complete the current frame.
    ///
    /// A completed frame accepts no further bytes until `next_frame` drains it.
    /// A header is validated before any body bytes are copied. On a framing,
    /// limit, or allocation error, retained input is cleared and the decoder
    /// rejects further operations until `reset`.
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize> {
        self.ensure_active()?;
        match self.push_inner(bytes) {
            Ok(consumed) => Ok(consumed),
            Err(error) => self.fail(error),
        }
    }

    fn push_inner(&mut self, bytes: &[u8]) -> Result<usize> {
        let mut consumed = 0;
        if self.buffer.len() < FRAME_HEADER_BYTES {
            consumed = bytes.len().min(FRAME_HEADER_BYTES - self.buffer.len());
            self.append(&bytes[..consumed], FRAME_HEADER_BYTES)?;
            if self.buffer.len() < FRAME_HEADER_BYTES {
                return Ok(consumed);
            }
        }

        let header = decode_header(&self.buffer, &self.limits)?;
        let total = frame_size(header.payload_len)?;
        let remaining = total.checked_sub(self.buffer.len()).ok_or_else(||
            wire_error(ErrorCode::FramingError, "buffer exceeds current frame length"))?;
        let body_bytes = remaining.min(bytes.len() - consumed);
        // Both endpoints lie in the caller's slice, so the sum cannot overflow.
        self.append(&bytes[consumed..consumed + body_bytes], total)?;
        Ok(consumed + body_bytes)
    }

    // Geometric growth avoids reallocating for each small push. Exact reserve
    // caps every requested allocation at the current validated frame size; an
    // incomplete header may request only five bytes. Allocator rounding is not
    // part of the logical retained-input bound.
    fn append(&mut self, bytes: &[u8], ceiling: usize) -> Result<()> {
        let needed = self.buffer.len().checked_add(bytes.len())
            .filter(|&length| length <= ceiling)
            .ok_or_else(|| wire_error(ErrorCode::FramingError, "frame buffer length exceeds bound"))?;
        if needed > self.buffer.capacity() {
            let target = self.buffer.capacity().saturating_mul(2).max(needed).min(ceiling);
            self.buffer.try_reserve_exact(target - self.buffer.len())
                .map_err(|_| wire_error(ErrorCode::BudgetExhausted, "frame buffer allocation failed"))?;
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Takes one complete frame, or returns `None` without discarding a prefix.
    ///
    /// The header is removed in place and the allocation is transferred through
    /// `Payload::from_vec`. No second payload buffer or borrowed input escapes.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        self.ensure_active()?;
        if self.buffer.len() < FRAME_HEADER_BYTES {
            return Ok(None);
        }
        let header = match decode_header(&self.buffer, &self.limits) {
            Ok(header) => header,
            Err(error) => return self.fail(error),
        };
        let total = match frame_size(header.payload_len) {
            Ok(total) => total,
            Err(error) => return self.fail(error),
        };
        if self.buffer.len() < total {
            return Ok(None);
        }
        if self.buffer.len() > total {
            return self.fail(wire_error(ErrorCode::FramingError, "buffer exceeds current frame length"));
        }
        let mut payload = core::mem::take(&mut self.buffer);
        payload.copy_within(FRAME_HEADER_BYTES.., 0);
        payload.truncate(total - FRAME_HEADER_BYTES);
        Ok(Some(Frame { header, payload: Payload::from_vec(payload) }))
    }

    /// Succeeds only when all input has been drained from a healthy decoder.
    ///
    /// A partial header, partial body, or undrained complete frame returns
    /// `TransportIncomplete` without changing state. More input can follow.
    pub fn finish(&self) -> Result<()> {
        self.ensure_active()?;
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(wire_error(ErrorCode::TransportIncomplete, "frame input remains undrained"))
        }
    }

    /// Number of currently retained header and payload bytes.
    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Discards current input and clears failure, preserving configured limits.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.failed = false;
    }

    fn ensure_active(&self) -> Result<()> {
        if self.failed {
            Err(wire_error(ErrorCode::FramingError, "frame decoder requires reset"))
        } else {
            Ok(())
        }
    }

    fn fail<T>(&mut self, error: Error) -> Result<T> {
        self.buffer.clear();
        self.failed = true;
        Err(error)
    }
}
