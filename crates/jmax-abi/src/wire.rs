//! Transport-independent envelope views and framing metadata.

use crate::span::{Span32, StrRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EnvelopeKind {
    EndpointRequest,
    EndpointImmediate,
    EndpointStream,
    EndpointError,
    EndpointLog,
    EndpointForwarded,
    EndpointClose,
    McpRequest,
    McpNotification,
    McpResponse,
    McpError,
}

/// Borrow-free envelope metadata referencing externally owned message storage.
///
/// Presence requirements depend on the envelope kind and are enforced by the
/// wire decoder. A missing field remains distinct from a present zero value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireEnvelopeView {
    pub kind: EnvelopeKind,
    pub method: Option<StrRef>,
    pub seq: Option<u64>,
    pub code: Option<u32>,
    pub params_span: Option<Span32>,
    pub result_span: Option<Span32>,
    pub error_span: Option<Span32>,
}

/// Decoded framing metadata, not a directly serializable memory layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramedHeader {
    pub payload_len: u32,
    pub frame_kind: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SpanSource;

    #[test]
    fn envelope_kinds_are_distinct_and_follow_the_frozen_order() {
        let kinds = [
            EnvelopeKind::EndpointRequest, EnvelopeKind::EndpointImmediate,
            EnvelopeKind::EndpointStream, EnvelopeKind::EndpointError,
            EnvelopeKind::EndpointLog, EnvelopeKind::EndpointForwarded,
            EnvelopeKind::EndpointClose, EnvelopeKind::McpRequest,
            EnvelopeKind::McpNotification, EnvelopeKind::McpResponse,
            EnvelopeKind::McpError,
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            assert_eq!(kind as usize, index);
            for other in &kinds[index + 1..] {
                assert_ne!(kind, *other);
            }
        }
    }

    #[test]
    fn absent_metadata_differs_from_present_zero_values() {
        let absent = WireEnvelopeView {
            kind: EnvelopeKind::EndpointClose, method: None, seq: None, code: None,
            params_span: None, result_span: None, error_span: None,
        };
        assert_eq!(absent, absent);
        assert_ne!(absent, WireEnvelopeView { seq: Some(0), ..absent });
        assert_ne!(absent, WireEnvelopeView { code: Some(0), ..absent });
        assert_ne!(absent, WireEnvelopeView {
            params_span: Some(Span32::new(0, 0)), ..absent
        });
        assert_ne!(absent, WireEnvelopeView {
            result_span: Some(Span32::new(0, 0)), ..absent
        });
        assert_ne!(absent, WireEnvelopeView {
            error_span: Some(Span32::new(0, 0)), ..absent
        });
        assert_ne!(absent, WireEnvelopeView {
            method: Some(StrRef {
                span: Span32::new(0, 0), source: SpanSource::RawPayload, scalar_len: 0,
            }), ..absent
        });
    }

    #[test]
    fn view_preserves_full_width_metadata_and_storage_sources() {
        let method = StrRef {
            span: Span32::new(4, 6), source: SpanSource::DecodedArena, scalar_len: 6,
        };
        let view = WireEnvelopeView {
            kind: EnvelopeKind::McpRequest, method: Some(method),
            seq: Some(u64::MAX), code: Some(u32::MAX),
            params_span: Some(Span32::new(10, 2)),
            result_span: Some(Span32::new(12, 4)),
            error_span: Some(Span32::new(16, 3)),
        };
        let copied = view;
        assert_eq!(view, copied);
        assert_eq!(copied.method, Some(method));
        assert_eq!(copied.seq, Some(u64::MAX));
        assert_eq!(copied.code, Some(u32::MAX));
        assert_eq!(copied.params_span, Some(Span32::new(10, 2)));
        assert_eq!(copied.result_span, Some(Span32::new(12, 4)));
        assert_eq!(copied.error_span, Some(Span32::new(16, 3)));
    }

    #[test]
    fn framed_header_preserves_zero_and_maximum_values() {
        for payload_len in [0, 1, u32::MAX] {
            for frame_kind in [0, 1, u8::MAX] {
                let header = FramedHeader { payload_len, frame_kind };
                let copied = header;
                assert_eq!(copied, header);
                assert_eq!(copied.payload_len, payload_len);
                assert_eq!(copied.frame_kind, frame_kind);
            }
        }
        assert_ne!(
            FramedHeader { payload_len: 0, frame_kind: 0 },
            FramedHeader { payload_len: 0, frame_kind: 1 },
        );
    }
}
