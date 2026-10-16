use jmax_abi::span::Span32;
use jmax_wss::*;
use std::sync::Arc;

struct Validator;
impl MessageValidator for Validator {
    fn validate(&mut self, payload: Payload, _: Limits) -> Result<ValidatedSlots> {
        assert_eq!(payload.as_slice(), b"null");
        // SAFETY: fixed test schema accepts this checked null literal. The
        // output has no decoded data, slots or unresolved references.
        let proof = unsafe { ValidationProof::issue_unchecked(7, 4, 0) };
        Ok(ValidatedSlots {
            payload,
            decoded: Vec::new(),
            slots: Vec::new(),
            proof,
        })
    }
}

#[test]
fn all_close_reason_variants_keep_the_original_allocation() -> Result<()> {
    let close = [0x03, 0xe8, b'b', b'y', b'e'];
    let variants = [
        Payload::Shared(Arc::from(&close[..])),
        Payload::from_arc_owner(Arc::new(close.to_vec()))?,
        Payload::Bytes(Bytes::copy_from_slice(&close)),
    ];
    for payload in variants {
        let original = payload.clone();
        let mut state = FragmentState::new();
        let event = receive::receive_frame(
            &mut state,
            IncomingFrame {
                opcode: Opcode::Close,
                final_fragment: true,
                payload,
            },
            &WssPolicy::default(),
        )?;
        match event {
            ReceiveEvent::Close { code, reason } => {
                assert_eq!(code, Some(1000));
                assert_eq!(reason.as_slice(), b"bye");
                assert_eq!(
                    reason.as_slice().as_ptr(),
                    original.as_slice().as_ptr().wrapping_add(2)
                );
                assert!(reason.shares_backing_with(&original));
            }
            _ => panic!("expected close event"),
        }
        assert!(state.is_closed());
    }
    Ok(())
}

#[test]
fn generic_arc_and_nested_subview_adopt_without_copying() -> Result<()> {
    let owner = Arc::new(b"null".to_vec());
    let pointer = owner.as_ptr();
    let input = receive::receive_arc_owner(owner, Opcode::Text, &WssPolicy::default())?;
    assert_eq!(input.payload().as_slice().as_ptr(), pointer);
    let job = receive::into_parse_job(
        input,
        &WssPolicy::default(),
        jmax_nagoya::CancellationToken::new(),
    )?;
    assert_eq!(job.payload.as_slice().as_ptr(), pointer);
    let backing = Payload::from_arc_owner(Arc::new(b"xxnullxx".to_vec()))?;
    let outer = backing.slice(1..7)?;
    let input = receive::receive_subview(
        &outer,
        Span32::new(1, 4),
        Opcode::Text,
        &WssPolicy::default(),
    )?;
    assert_eq!(input.payload().as_slice(), b"null");
    assert!(input.payload().shares_backing_with(&backing));
    assert!(
        receive::receive_subview(
            &outer,
            Span32::new(5, 5),
            Opcode::Text,
            &WssPolicy::default()
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn partial_write_cursor_preserves_both_owners_and_never_reencodes() -> Result<()> {
    let source = Arc::new(b"null".to_vec());
    let source_weak = Arc::downgrade(&source);
    let output = send::prepare_output(
        Payload::from_arc_owner(source)?,
        Opcode::Text,
        &mut Validator,
        &WssPolicy::default(),
    )?;
    let encoded = Arc::new(vec![0x81, 4, b'n', b'u', b'l', b'l']);
    let encoded_weak = Arc::downgrade(&encoded);
    let pointer = encoded.as_ptr();
    let mut frame = send::OutboundFrame::new(output, Payload::from_arc_owner(encoded)?);
    assert_eq!(frame.remaining().as_ptr(), pointer);
    frame.advance(1)?;
    frame.advance(0)?;
    frame.advance(1)?;
    assert_eq!(frame.remaining(), b"null");
    let view = frame.remaining_payload()?;
    assert_eq!(view.as_slice().as_ptr(), pointer.wrapping_add(2));
    assert!(frame.advance(5).is_err());
    assert_eq!(frame.offset(), 2);
    frame.advance(4)?;
    assert!(frame.is_complete());
    assert!(frame.remaining().is_empty());
    assert!(source_weak.upgrade().is_some());
    let (output, encoded, offset) = frame.into_parts();
    assert_eq!(offset, 6);
    drop(output);
    assert!(source_weak.upgrade().is_none());
    drop(encoded);
    assert!(encoded_weak.upgrade().is_some()); // the pending subview still owns it
    drop(view);
    assert!(encoded_weak.upgrade().is_none());
    Ok(())
}
