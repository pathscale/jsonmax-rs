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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn limits(max_payload_bytes: u32, kinds: &[u8]) -> FrameLimits {
        FrameLimits { max_payload_bytes, allowed_kinds: kinds.to_vec() }
    }

    fn frame(kind: u8, payload: &[u8]) -> Frame {
        Frame {
            header: FramedHeader {
                payload_len: u32::try_from(payload.len()).unwrap(), frame_kind: kind,
            },
            payload: Payload::from_slice(payload),
        }
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Wire);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        error
    }

    fn assert_frame(actual: Frame, kind: u8, payload: &[u8]) {
        assert_eq!(actual.header.frame_kind, kind);
        assert_eq!(actual.header.payload_len as usize, payload.len());
        assert_eq!(actual.payload.as_slice(), payload);
    }

    #[test]
    fn header_vectors_are_exactly_little_endian_and_ignore_trailing_bytes() {
        let limits = limits(0x1234_5678, &[0, 0x7f, 0xff]);
        for (bytes, length, kind) in [
            ([1, 0, 0, 0, 0], 1, 0),
            ([0, 1, 0, 0, 0x7f], 256, 0x7f),
            ([0, 0, 1, 0, 0xff], 65_536, 0xff),
            ([0x78, 0x56, 0x34, 0x12, 0x7f], 0x1234_5678, 0x7f),
        ] {
            let expected = FramedHeader { payload_len: length, frame_kind: kind };
            assert_eq!(decode_header(&bytes, &limits).unwrap(), expected);
            let mut with_payload = bytes.to_vec();
            with_payload.extend_from_slice(&[0xff, 0, 3, 4, 5, 6]);
            assert_eq!(decode_header(&with_payload, &limits).unwrap(), expected);
        }
    }

    #[test]
    fn every_short_header_is_incomplete_even_when_its_prefix_is_invalid() {
        let limits = limits(3, &[9]);
        for bytes in [[0; 5], [0xff; 5], [1, 0, 0, 0, 9]] {
            for length in 0..FRAME_HEADER_BYTES {
                assert_error(decode_header(&bytes[..length], &limits), ErrorCode::TransportIncomplete);
            }
        }
    }

    #[test]
    fn invalid_configuration_is_rejected_by_every_entry_point_before_writing() {
        for config in [limits(0, &[1]), limits(4, &[1, 2, 1]), limits(4, &[255, 255])] {
            assert_error(FrameDecoder::new(config.clone()), ErrorCode::FramingError);
            assert_error(decode_header(&[1, 0, 0, 0, 1], &config), ErrorCode::FramingError);
            let mut output = b"prefix".to_vec();
            assert_error(encode_frame(&frame(1, b"x"), &config, &mut output), ErrorCode::FramingError);
            assert_eq!(output, b"prefix");
        }
    }

    #[test]
    fn payload_limit_is_inclusive_and_zero_length_is_forbidden() {
        let limits = limits(4, &[7]);
        assert_error(decode_header(&[0, 0, 0, 0, 7], &limits), ErrorCode::FramingError);
        assert_eq!(decode_header(&[1, 0, 0, 0, 7], &limits).unwrap().payload_len, 1);
        assert_eq!(decode_header(&[4, 0, 0, 0, 7], &limits).unwrap().payload_len, 4);
        for bytes in [[5, 0, 0, 0, 7], [0xff, 0xff, 0xff, 0xff, 7]] {
            assert_error(decode_header(&bytes, &limits), ErrorCode::PayloadTooLarge);
        }
    }

    #[test]
    fn all_256_kinds_are_opaque_allowlist_values() {
        let all = FrameLimits { max_payload_bytes: 1, allowed_kinds: (0..=u8::MAX).collect() };
        assert!(FrameDecoder::new(all.clone()).is_ok());
        let denied = limits(1, &[]);
        let mut empty = FrameDecoder::new(denied.clone()).unwrap();
        assert_eq!(empty.push(&[]).unwrap(), 0);
        assert!(empty.next_frame().unwrap().is_none());
        empty.finish().unwrap();
        for kind in 0..=u8::MAX {
            assert_eq!(decode_header(&[1, 0, 0, 0, kind], &all).unwrap().frame_kind, kind);
            assert_error(decode_header(&[1, 0, 0, 0, kind], &denied), ErrorCode::FramingError);
            let mut decoder = FrameDecoder::new(limits(1, &[kind])).unwrap();
            assert_eq!(decoder.push(&[1, 0, 0, 0, kind, kind]).unwrap(), 6);
            assert_frame(decoder.next_frame().unwrap().unwrap(), kind, &[kind]);
        }
        let mut decoder = FrameDecoder::new(denied).unwrap();
        assert_error(decoder.push(&[1, 0, 0, 0, 0, 99]), ErrorCode::FramingError);
        assert_eq!(decoder.buffered_len(), 0);
    }

    #[test]
    fn maximum_configuration_uses_checked_address_space_arithmetic() {
        let config = limits(u32::MAX, &[255]);
        let representable = usize::try_from(u32::MAX).ok()
            .and_then(|length| length.checked_add(FRAME_HEADER_BYTES));
        match representable {
            Some(total) => {
                assert_eq!(frame_size(u32::MAX).unwrap(), total);
                let decoder = FrameDecoder::new(config.clone()).unwrap();
                assert_eq!(decoder.buffered_len(), 0);
                assert_eq!(decoder.buffer.capacity(), 0);
                assert_eq!(decode_header(&[255; 5], &config).unwrap().payload_len, u32::MAX);
            }
            None => {
                assert_error(frame_size(u32::MAX), ErrorCode::FramingError);
                assert_error(FrameDecoder::new(config.clone()), ErrorCode::FramingError);
                assert_error(decode_header(&[255; 5], &config), ErrorCode::FramingError);
            }
        }
        if let Ok(largest) = u32::try_from(usize::MAX - FRAME_HEADER_BYTES) {
            let boundary = limits(largest, &[255]);
            assert_eq!(frame_size(largest).unwrap(), usize::MAX);
            assert!(FrameDecoder::new(boundary.clone()).is_ok());
            let length = largest.to_le_bytes();
            assert_eq!(decode_header(&[length[0], length[1], length[2], length[3], 255],
                &boundary).unwrap().payload_len, largest);
            if let Some(overflow) = largest.checked_add(1) {
                assert_error(FrameDecoder::new(limits(overflow, &[255])), ErrorCode::FramingError);
            }
        }
    }

    #[test]
    fn encode_vectors_preserve_binary_payload_and_append_without_padding() {
        let config = limits(256, &[0, 0xa5, 255]);
        for (kind, payload) in [(0, &b"x"[..]), (0xa5, &[0, 255, 128, 10][..]), (255, &b"{}"[..])] {
            let mut output = b"prefix".to_vec();
            encode_frame(&frame(kind, payload), &config, &mut output).unwrap();
            let mut expected = b"prefix".to_vec();
            expected.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            expected.push(kind);
            expected.extend_from_slice(payload);
            assert_eq!(output, expected);
        }
        let payload: Vec<u8> = (0..=255).collect();
        let mut output = Vec::new();
        encode_frame(&frame(255, &payload), &config, &mut output).unwrap();
        assert_eq!(&output[..5], &[0, 1, 0, 0, 255]);
        assert_eq!(&output[5..], payload.as_slice());
    }

    #[test]
    fn encoding_preserves_both_payload_storage_variants_and_their_owners() {
        let config = limits(3, &[6]);
        for payload in [Payload::from_slice(&[0, 128, 255]),
            Payload::from_vec(vec![0, 128, 255])]
        {
            let pointer = payload.as_slice().as_ptr();
            let retained = payload.clone();
            let input = Frame { header: FramedHeader { payload_len: 3, frame_kind: 6 }, payload };
            for _ in 0..2 {
                let mut output = Vec::new();
                encode_frame(&input, &config, &mut output).unwrap();
                assert_eq!(output, [3, 0, 0, 0, 6, 0, 128, 255]);
                assert_eq!(input.payload.as_slice().as_ptr(), pointer);
                assert_eq!(retained.as_slice(), &[0, 128, 255]);
            }
            drop(input);
            assert_eq!(retained.as_slice(), &[0, 128, 255]);
        }
    }

    #[test]
    fn encoding_rejects_all_invalid_frames_without_touching_the_sink() {
        let config = limits(4, &[7]);
        let mut short = frame(7, b"ab");
        short.header.payload_len = 3;
        let mut long = frame(7, b"abc");
        long.header.payload_len = 2;
        for (invalid, code) in [
            (frame(7, b""), ErrorCode::FramingError),
            (frame(7, b"abcde"), ErrorCode::PayloadTooLarge),
            (frame(8, b"a"), ErrorCode::FramingError),
            (short, ErrorCode::FramingError),
            (long, ErrorCode::FramingError),
        ] {
            let mut sink = FailingSink::new(0);
            assert_error(encode_frame(&invalid, &config, &mut sink), code);
            assert_eq!(sink.calls, 0);
            assert!(sink.bytes.is_empty());
        }
        let mut sink = FailingSink::new(0);
        assert_error(encode_frame(&frame(7, b"a"), &limits(4, &[]), &mut sink), ErrorCode::FramingError);
        assert_eq!(sink.calls, 0);
    }

    struct FailingSink {
        bytes: Vec<u8>,
        fail_after: usize,
        calls: usize,
        error: Error,
    }

    impl FailingSink {
        fn new(fail_after: usize) -> Self {
            Self {
                bytes: Vec::new(), fail_after, calls: 0,
                error: Error::new(ErrorCode::Cancelled, ErrorSource::Transport, 123,
                    "deterministic sink failure").with_schema(7, 11),
            }
        }
    }

    impl ByteSink for FailingSink {
        fn reserve(&mut self, additional: usize) -> Result<()> {
            self.calls += 1;
            if additional > self.fail_after.saturating_sub(self.bytes.len()) {
                Err(self.error.clone())
            } else {
                Ok(())
            }
        }

        fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
            self.calls += 1;
            let count = bytes.len().min(self.fail_after.saturating_sub(self.bytes.len()));
            self.bytes.extend_from_slice(&bytes[..count]);
            if count < bytes.len() { Err(self.error.clone()) } else { Ok(()) }
        }

        fn len(&self) -> usize { self.bytes.len() }
    }

    #[test]
    fn every_sink_failure_preserves_exact_error_metadata_and_output_prefix() {
        let input = frame(9, b"abc");
        let config = limits(3, &[9]);
        let expected = [3, 0, 0, 0, 9, b'a', b'b', b'c'];
        for failure_offset in 0..expected.len() {
            let mut sink = FailingSink::new(failure_offset);
            assert_eq!(encode_frame(&input, &config, &mut sink).unwrap_err(), sink.error);
            assert_eq!(sink.bytes, expected[..failure_offset]);
            assert_eq!(sink.calls, if failure_offset < 5 { 1 } else { 2 });
        }
        let mut sink = FailingSink::new(expected.len());
        encode_frame(&input, &config, &mut sink).unwrap();
        assert_eq!(sink.bytes, expected);
    }

    #[test]
    fn every_two_chunk_split_and_single_byte_feed_produce_the_same_frame() {
        let wire = [4, 0, 0, 0, 42, 0, 255, 128, 10];
        for split in 0..=wire.len() {
            let mut decoder = FrameDecoder::new(limits(4, &[42])).unwrap();
            assert_eq!(decoder.push(&wire[..split]).unwrap(), split);
            assert_eq!(decoder.buffered_len(), split);
            if split < wire.len() {
                assert!(decoder.next_frame().unwrap().is_none());
            }
            assert_eq!(decoder.push(&wire[split..]).unwrap(), wire.len() - split);
            assert_frame(decoder.next_frame().unwrap().unwrap(), 42, &wire[5..]);
            assert_eq!(decoder.buffered_len(), 0);
            assert!(decoder.next_frame().unwrap().is_none());
            decoder.finish().unwrap();
        }
        let mut decoder = FrameDecoder::new(limits(4, &[42])).unwrap();
        for (index, byte) in wire.iter().enumerate() {
            assert_eq!(decoder.push(core::slice::from_ref(byte)).unwrap(), 1);
            assert_eq!(decoder.buffered_len(), index + 1);
            if index + 1 < wire.len() {
                assert!(decoder.next_frame().unwrap().is_none());
            }
        }
        assert_frame(decoder.next_frame().unwrap().unwrap(), 42, &wire[5..]);
    }

    #[test]
    fn concatenated_frames_return_precise_consumption_and_backpressure() {
        let wire = [2, 0, 0, 0, 1, b'a', b'b', 1, 0, 0, 0, 2, 255];
        let mut decoder = FrameDecoder::new(limits(2, &[1, 2])).unwrap();
        let consumed = decoder.push(&wire).unwrap();
        assert_eq!(consumed, 7);
        assert_eq!(decoder.buffered_len(), 7);
        assert_eq!(decoder.push(&wire[consumed..]).unwrap(), 0);
        assert_eq!(decoder.push(&[]).unwrap(), 0);
        assert_error(decoder.finish(), ErrorCode::TransportIncomplete);
        assert_frame(decoder.next_frame().unwrap().unwrap(), 1, b"ab");
        assert_eq!(decoder.push(&wire[consumed..]).unwrap(), 6);
        assert_frame(decoder.next_frame().unwrap().unwrap(), 2, &[255]);
        decoder.finish().unwrap();
    }

    #[test]
    fn a_partial_header_still_stops_exactly_at_the_first_frame_boundary() {
        let wire = [2, 0, 0, 0, 1, 7, 8, 0xff, 0xff, 0xff, 0xff, 255];
        for prefix in 1..7 {
            let mut decoder = FrameDecoder::new(limits(2, &[1])).unwrap();
            assert_eq!(decoder.push(&wire[..prefix]).unwrap(), prefix);
            assert_eq!(decoder.push(&wire[prefix..]).unwrap(), 7 - prefix);
            assert_frame(decoder.next_frame().unwrap().unwrap(), 1, &[7, 8]);
            decoder.finish().unwrap();
            assert_error(decoder.push(&wire[7..]), ErrorCode::PayloadTooLarge);
            assert_eq!(decoder.buffered_len(), 0);
        }
    }

    #[test]
    fn finish_preserves_every_incomplete_prefix_and_allows_resumption() {
        let wire = [3, 0, 0, 0, 4, b'a', b'b', b'c'];
        for prefix in 0..wire.len() {
            let mut decoder = FrameDecoder::new(limits(3, &[4])).unwrap();
            assert_eq!(decoder.push(&wire[..prefix]).unwrap(), prefix);
            for _ in 0..2 {
                if prefix == 0 { decoder.finish().unwrap(); }
                else { assert_error(decoder.finish(), ErrorCode::TransportIncomplete); }
                assert_eq!(decoder.buffered_len(), prefix);
                assert_eq!(decoder.push(&[]).unwrap(), 0);
                assert!(decoder.next_frame().unwrap().is_none());
            }
            assert_eq!(decoder.push(&wire[prefix..]).unwrap(), wire.len() - prefix);
            assert_frame(decoder.next_frame().unwrap().unwrap(), 4, b"abc");
            decoder.finish().unwrap();
        }
    }

    #[test]
    fn header_errors_clear_input_and_remain_failed_until_reset() {
        for (invalid, code) in [
            ([0, 0, 0, 0, 7], ErrorCode::FramingError),
            ([5, 0, 0, 0, 7], ErrorCode::PayloadTooLarge),
            ([1, 0, 0, 0, 8], ErrorCode::FramingError),
        ] {
            for prefix in 0..5 {
                let config = limits(4, &[7]);
                let mut decoder = FrameDecoder::new(config.clone()).unwrap();
                decoder.push(&invalid[..prefix]).unwrap();
                let mut rest = invalid[prefix..].to_vec();
                rest.extend_from_slice(&[99; 32]);
                assert_error(decoder.push(&rest), code);
                assert_eq!(decoder.buffered_len(), 0);
                assert!(decoder.failed);
                for _ in 0..2 {
                    assert_error(decoder.push(&[]), ErrorCode::FramingError);
                    assert_error(decoder.push(&[1, 0, 0, 0, 7, 9]), ErrorCode::FramingError);
                    assert_error(decoder.next_frame(), ErrorCode::FramingError);
                    assert_error(decoder.finish(), ErrorCode::FramingError);
                }
                decoder.reset();
                assert_eq!(decoder.limits, config);
                assert_eq!(decoder.buffered_len(), 0);
                decoder.finish().unwrap();
                assert_eq!(decoder.push(&[1, 0, 0, 0, 7, 9]).unwrap(), 6);
                assert_frame(decoder.next_frame().unwrap().unwrap(), 7, &[9]);
            }
        }
    }

    #[test]
    fn reset_discards_partial_and_complete_frames_without_changing_limits() {
        let wire = [3, 0, 0, 0, 2, 11, 12, 13];
        for prefix in 0..=wire.len() {
            let config = limits(3, &[2]);
            let mut decoder = FrameDecoder::new(config.clone()).unwrap();
            decoder.push(&wire[..prefix]).unwrap();
            decoder.reset();
            decoder.reset();
            assert_eq!(decoder.limits, config);
            assert_eq!(decoder.buffered_len(), 0);
            assert!(decoder.next_frame().unwrap().is_none());
            decoder.finish().unwrap();
            assert_eq!(decoder.push(&wire).unwrap(), wire.len());
            assert_frame(decoder.next_frame().unwrap().unwrap(), 2, &[11, 12, 13]);
        }
    }

    #[test]
    fn input_is_copied_and_completed_payloads_outlive_decoder_and_reset() {
        let mut input = [3, 0, 0, 0, 1, 0, 128, 255];
        let mut decoder = FrameDecoder::new(limits(3, &[1])).unwrap();
        decoder.push(&input).unwrap();
        input.fill(0);
        let allocation = decoder.buffer.as_ptr();
        let first = decoder.next_frame().unwrap().unwrap();
        assert_eq!(first.payload.as_slice().as_ptr(), allocation);
        assert_eq!(decoder.buffer.capacity(), 0);
        decoder.push(&[1, 0, 0, 0, 1, 42]).unwrap();
        let second = decoder.next_frame().unwrap().unwrap();
        decoder.reset();
        drop(decoder);
        assert_frame(first, 1, &[0, 128, 255]);
        assert_frame(second, 1, &[42]);
    }

    #[test]
    fn round_trips_at_geometric_growth_and_header_length_boundaries() {
        for length in [1usize, 2, 4, 5, 6, 9, 10, 11, 15, 16, 31, 32, 255, 256, 257, 1024] {
            let payload: Vec<u8> = (0..length).map(|index| (index % 256) as u8).collect();
            let config = limits(length as u32, &[0xfe]);
            let mut wire = Vec::new();
            encode_frame(&frame(0xfe, &payload), &config, &mut wire).unwrap();
            for chunk_size in [1, 2, 3, 5, 7, 64, 257] {
                let mut decoder = FrameDecoder::new(config.clone()).unwrap();
                for chunk in wire.chunks(chunk_size) {
                    assert_eq!(decoder.push(chunk).unwrap(), chunk.len());
                    assert!(decoder.buffered_len() <= length + FRAME_HEADER_BYTES);
                }
                assert_frame(decoder.next_frame().unwrap().unwrap(), 0xfe, &payload);
                assert_eq!(decoder.buffered_len(), 0);
                decoder.finish().unwrap();
            }
        }
    }

    #[test]
    fn internal_bound_failures_preserve_bytes_until_the_public_boundary_poisons() {
        let mut decoder = FrameDecoder::new(limits(1, &[1])).unwrap();
        decoder.append(&[1, 2], 5).unwrap();
        assert_error(decoder.append(&[3, 4, 5, 6], 5), ErrorCode::FramingError);
        assert_eq!(decoder.buffer, vec![1, 2]);
        let error = wire_error(ErrorCode::BudgetExhausted, "deterministic allocation failure");
        assert_eq!(decoder.fail::<()>(error.clone()).unwrap_err(), error);
        assert_eq!(decoder.buffered_len(), 0);
        assert_error(decoder.next_frame(), ErrorCode::FramingError);
        decoder.reset();
        decoder.finish().unwrap();
    }
}
