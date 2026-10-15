//! Container stack updates, inclusive resource limits, and decoded key identity.
//!
//! All fallible work precedes structural mutation. A failed reservation may
//! change a vector's capacity, but never its contents, counters, or phases.

use super::*;

/// Returns the innermost container phase, or the root phase without a frame.
pub fn current_phase(state: &SyntaxState) -> FramePhase {
    state.frames.last().map_or(state.root_phase, |frame| frame.phase)
}

/// Checks retained payload growth against both the ABI and configured ceiling.
///
/// The error offset is the current length, saturated to the ABI's offset range.
/// This function performs no allocation and does not mutate parser state.
pub fn check_payload_growth(current: usize, additional: usize, limits: Limits) -> Result<u32> {
    let offset = u32::try_from(current).unwrap_or(u32::MAX);
    current
        .checked_add(additional)
        .and_then(|total| u32::try_from(total).ok())
        .filter(|&total| total <= limits.max_payload_bytes)
        .ok_or_else(|| syntax_error(
            ErrorCode::PayloadTooLarge,
            offset,
            "payload exceeds its byte limit or offset range",
        ))
}

/// Applies the machine's transition for this exact token and current phase.
///
/// Feed owns lexical validation, offsets, pending tokens, and event delivery.
/// Container starts count immediately in arrays; object fields count at keys.
/// The first failure is retained, including failures originating in other stages.
pub(crate) fn apply(
    state: &mut SyntaxState,
    token: Token,
    transition: Transition,
) -> Result<()> {
    if let Some(error) = state.failure.as_ref() {
        return Err(error.clone());
    }
    match apply_inner(state, token, transition) {
        Ok(()) => Ok(()),
        Err(error) => Err(state.fail(error)),
    }
}

fn apply_inner(state: &mut SyntaxState, token: Token, transition: Transition) -> Result<()> {
    let offset = token.span.offset;
    let inconsistent = || syntax_error(
        ErrorCode::InternalError,
        offset,
        "inconsistent syntax frame or transition",
    );

    if !matches!(state.root_phase, FramePhase::RootStart | FramePhase::RootDone) {
        return Err(inconsistent());
    }
    if let Some(frame) = state.frames.last() {
        let valid_phase = match frame.kind {
            ContainerKind::Object => matches!(
                frame.phase,
                FramePhase::ObjectStart
                    | FramePhase::ObjectKeyOrEnd
                    | FramePhase::ObjectColon
                    | FramePhase::ObjectValue
                    | FramePhase::ObjectCommaOrEnd
            ),
            ContainerKind::Array => matches!(
                frame.phase,
                FramePhase::ArrayStart | FramePhase::ArrayValueOrEnd | FramePhase::ArrayCommaOrEnd
            ),
        };
        if !valid_phase || state.root_phase != FramePhase::RootDone {
            return Err(inconsistent());
        }
    }

    // Reusing the pure, allocation-free machine keeps defensive validation in
    // sync with its grammar and catches stale or mismatched internal calls.
    let expected = machine::transition(current_phase(state), token)
        .map_err(|_| inconsistent())?;
    if transition != expected {
        return Err(inconsistent());
    }

    if transition.action == FrameAction::Pop {
        let frame = state.frames.last().ok_or_else(inconsistent)?;
        if !matches!(
            (frame.kind, token.kind),
            (ContainerKind::Object, TokenKind::RBrace)
                | (ContainerKind::Array, TokenKind::RBracket)
        ) {
            return Err(inconsistent());
        }
        // The departing next_phase must not overwrite the saved parent phase.
        return state.frames.pop().map(|_| ()).ok_or_else(inconsistent);
    }

    let child = match transition.action {
        FrameAction::PushObject => Some((ContainerKind::Object, FramePhase::ObjectStart)),
        FrameAction::PushArray => Some((ContainerKind::Array, FramePhase::ArrayStart)),
        FrameAction::None => None,
        FrameAction::Pop => return Err(inconsistent()),
    };
    if child.is_some() && state.frames.len() >= usize::from(state.limits.max_depth) {
        return Err(syntax_error(
            ErrorCode::DepthLimitExceeded,
            offset,
            "container depth exceeds its limit",
        ));
    }

    let key = match transition.event {
        Some(Event::Key { str_ref }) => Some(str_ref),
        _ => None,
    };
    let next_count = match state.frames.last() {
        Some(frame) if key.is_some() => {
            if frame.kind != ContainerKind::Object {
                return Err(inconsistent());
            }
            Some(increment_count(
                frame.count,
                state.limits.max_fields_per_object,
                offset,
                "object field count exceeds its limit",
            )?)
        }
        Some(frame)
            if frame.kind == ContainerKind::Array && machine::is_value_start(token.kind) =>
        {
            Some(increment_count(
                frame.count,
                state.limits.max_array_items,
                offset,
                "array item count exceeds its limit",
            )?)
        }
        None if key.is_some() => return Err(inconsistent()),
        _ => None,
    };

    let mut owned_key = None;
    if state.limits.reject_duplicate_keys {
        if let Some(reference) = key {
            let bytes = state.resolve_str(reference).ok_or_else(inconsistent)?.as_bytes();
            let frame = state.frames.last().ok_or_else(inconsistent)?;
            if frame.seen_keys.iter().any(|seen| seen.as_slice() == bytes) {
                return Err(syntax_error(
                    ErrorCode::DuplicateObjectKey,
                    offset,
                    "duplicate decoded object key",
                ));
            }
            let mut copy = Vec::new();
            reserve(&mut copy, bytes.len(), offset)?;
            copy.extend_from_slice(bytes);
            let frame = state.frames.last_mut().ok_or_else(inconsistent)?;
            reserve(&mut frame.seen_keys, 1, offset)?;
            owned_key = Some(copy);
        }
    }
    if child.is_some() {
        reserve(&mut state.frames, 1, offset)?;
    }

    // All checks and reservations have succeeded. The following pushes cannot
    // allocate. Unknown-key policy belongs to schema validation, not framing.
    match state.frames.last_mut() {
        Some(frame) => {
            frame.phase = transition.next_phase;
            if let Some(count) = next_count {
                frame.count = count;
            }
            if let Some(bytes) = owned_key {
                frame.seen_keys.push(bytes);
            }
        }
        None => {
            if next_count.is_some() || owned_key.is_some() {
                return Err(inconsistent());
            }
            state.root_phase = transition.next_phase;
        }
    }
    if let Some((kind, phase)) = child {
        state.frames.push(Frame {
            kind,
            phase,
            start_offset: offset,
            count: 0,
            seen_keys: Vec::new(),
        });
    }
    Ok(())
}

fn increment_count(count: u32, limit: u32, offset: u32, detail: &'static str) -> Result<u32> {
    count.checked_add(1).filter(|&next| next <= limit).ok_or_else(|| {
        syntax_error(ErrorCode::ConstraintViolation, offset, detail)
    })
}

fn reserve<T>(storage: &mut Vec<T>, additional: usize, offset: u32) -> Result<()> {
    storage.try_reserve(additional).map_err(|_| syntax_error(
        ErrorCode::BudgetExhausted,
        offset,
        "unable to reserve syntax frame storage",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct Snapshot {
        root: FramePhase,
        frames: Vec<(ContainerKind, FramePhase, u32, u32, Vec<Vec<u8>>)>,
        input: Vec<u8>,
        decoded: Vec<u8>,
        offset: u32,
        pending: Option<PendingToken>,
        final_input: bool,
        finished: bool,
        limits: Limits,
    }

    fn snapshot(state: &SyntaxState) -> Snapshot {
        Snapshot {
            root: state.root_phase,
            frames: state.frames.iter().map(|frame| (
                frame.kind, frame.phase, frame.start_offset, frame.count, frame.seen_keys.clone(),
            )).collect(),
            input: state.input.clone(),
            decoded: state.decoded.clone(),
            offset: state.byte_offset,
            pending: state.pending,
            final_input: state.final_input,
            finished: state.finished,
            limits: state.limits,
        }
    }

    fn token(kind: TokenKind, offset: u32) -> Token {
        Token {
            kind,
            span: Span32::new(offset, match kind {
                TokenKind::True | TokenKind::Null => 4,
                TokenKind::False => 5,
                TokenKind::String => 2,
                _ => 1,
            }),
            decoded: if kind == TokenKind::String {
                Some(StrRef {
                    span: Span32::new(offset + 1, 0),
                    source: SpanSource::RawPayload,
                    scalar_len: 0,
                })
            } else {
                None
            },
        }
    }

    fn string_token(offset: u32, raw_len: u32, reference: StrRef) -> Token {
        Token { kind: TokenKind::String, span: Span32::new(offset, raw_len), decoded: Some(reference) }
    }

    fn accept(state: &mut SyntaxState, token: Token) {
        let transition = machine::transition(current_phase(state), token).unwrap();
        apply(state, token, transition).unwrap();
    }

    fn step(state: &mut SyntaxState, kind: TokenKind, offset: u32) {
        accept(state, token(kind, offset));
    }

    fn assert_error(error: &Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Syntax);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn reject_transition(
        state: &mut SyntaxState,
        token: Token,
        transition: Transition,
        code: ErrorCode,
    ) {
        let before = snapshot(state);
        let error = apply(state, token, transition).unwrap_err();
        assert_error(&error, code, token.span.offset);
        assert_eq!(snapshot(state), before);
        assert_eq!(state.failure(), Some(&error));
        assert_eq!(apply(state, token, transition), Err(error));
        assert_eq!(snapshot(state), before);
    }

    fn reject(state: &mut SyntaxState, token: Token, code: ErrorCode) {
        let transition = machine::transition(current_phase(state), token).unwrap();
        reject_transition(state, token, transition, code);
    }

    fn raw_key(offset: u32, content: &str) -> Token {
        let len = u32::try_from(content.len()).unwrap();
        string_token(offset, len + 2, StrRef {
            span: Span32::new(offset + 1, len),
            source: SpanSource::RawPayload,
            scalar_len: u32::try_from(content.chars().count()).unwrap(),
        })
    }

    fn next_field(state: &mut SyntaxState) {
        step(state, TokenKind::Colon, 100);
        step(state, TokenKind::Number, 101);
        step(state, TokenKind::Comma, 102);
    }

    #[test]
    fn payload_limits_are_inclusive_and_zero_is_not_unlimited() {
        for (current, additional, limit, expected) in [
            (0, 0, 0, Some(0)),
            (0, 1, 0, None),
            (0, 4, 4, Some(4)),
            (3, 1, 4, Some(4)),
            (4, 0, 4, Some(4)),
            (4, 1, 4, None),
            (5, 0, 4, None),
        ] {
            let limits = Limits { max_payload_bytes: limit, ..Limits::strict_default() };
            match expected {
                Some(total) => assert_eq!(check_payload_growth(current, additional, limits), Ok(total)),
                None => assert_error(
                    &check_payload_growth(current, additional, limits).unwrap_err(),
                    ErrorCode::PayloadTooLarge,
                    u32::try_from(current).unwrap(),
                ),
            }
        }
    }

    #[test]
    fn payload_overflow_and_offset_saturation_require_no_large_allocation() {
        let limits = Limits::permissive_default();
        assert_error(
            &check_payload_growth(1, usize::MAX, limits).unwrap_err(),
            ErrorCode::PayloadTooLarge, 1,
        );
        assert_error(
            &check_payload_growth(usize::MAX, 1, limits).unwrap_err(),
            ErrorCode::PayloadTooLarge, u32::try_from(usize::MAX).unwrap_or(u32::MAX),
        );
        if let Ok(max) = usize::try_from(u32::MAX) {
            assert_eq!(check_payload_growth(max - 1, 1, limits), Ok(u32::MAX));
            assert_eq!(check_payload_growth(max, 0, limits), Ok(u32::MAX));
            assert_error(
                &check_payload_growth(max, 1, limits).unwrap_err(),
                ErrorCode::PayloadTooLarge, u32::MAX,
            );
        }
        if let Ok(too_large) = usize::try_from(u64::from(u32::MAX) + 1) {
            for additional in [0, 1, usize::MAX] {
                assert_error(
                    &check_payload_growth(too_large, additional, limits).unwrap_err(),
                    ErrorCode::PayloadTooLarge, u32::MAX,
                );
            }
        }
    }

    #[test]
    fn current_phase_tracks_containers_and_restores_the_root() {
        let mut state = SyntaxState::new(Limits::strict_default());
        assert_eq!(current_phase(&state), FramePhase::RootStart);
        assert_eq!(state.state().depth, 0);
        step(&mut state, TokenKind::LBracket, 5);
        assert_eq!(state.root_phase, FramePhase::RootDone);
        assert_eq!(current_phase(&state), FramePhase::ArrayStart);
        assert_eq!(state.state().phase, Some(current_phase(&state)));
        let frame = state.frames.last().unwrap();
        assert_eq!(frame.kind(), ContainerKind::Array);
        assert_eq!(frame.phase(), FramePhase::ArrayStart);
        assert_eq!(frame.start_offset(), 5);
        assert_eq!(frame.count(), 0);
        assert!(frame.seen_keys.is_empty());
        step(&mut state, TokenKind::RBracket, 6);
        assert_eq!(current_phase(&state), FramePhase::RootDone);
        assert_eq!(state.state().depth, 0);
        assert!(!state.is_finished());
    }

    #[test]
    fn zero_depth_accepts_every_scalar_root_and_rejects_both_container_kinds() {
        let limits = Limits { max_depth: 0, ..Limits::strict_default() };
        for kind in [TokenKind::String, TokenKind::Number, TokenKind::True, TokenKind::False, TokenKind::Null] {
            let mut state = SyntaxState::new(limits);
            step(&mut state, kind, 0);
            assert_eq!(current_phase(&state), FramePhase::RootDone);
            assert!(state.frames.is_empty());
        }
        for kind in [TokenKind::LBrace, TokenKind::LBracket] {
            let mut state = SyntaxState::new(limits);
            reject(&mut state, token(kind, 17), ErrorCode::DepthLimitExceeded);
        }
    }

    #[test]
    fn zero_counts_allow_empty_containers_but_no_keys_or_items() {
        let limits = Limits {
            max_fields_per_object: 0, max_array_items: 0, ..Limits::strict_default()
        };
        for (open, close, disallowed) in [
            (TokenKind::LBrace, TokenKind::RBrace, TokenKind::String),
            (TokenKind::LBracket, TokenKind::RBracket, TokenKind::Null),
        ] {
            let mut empty = SyntaxState::new(limits);
            step(&mut empty, open, 0);
            step(&mut empty, close, 1);
            assert!(empty.frames.is_empty());
            let mut nonempty = SyntaxState::new(limits);
            step(&mut nonempty, open, 0);
            reject(&mut nonempty, token(disallowed, 1), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn depth_boundary_does_not_advance_or_count_a_rejected_child() {
        let mut state = SyntaxState::new(Limits { max_depth: 2, ..Limits::strict_default() });
        step(&mut state, TokenKind::LBracket, 0);
        step(&mut state, TokenKind::LBracket, 1);
        assert_eq!(state.state().depth, 2);
        assert_eq!(state.frames.first().unwrap().count(), 1);
        assert_eq!(state.frames.last().unwrap().count(), 0);
        reject(&mut state, token(TokenKind::LBrace, 2), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn maximum_abi_depth_is_inclusive_and_rejects_the_next_push() {
        let mut state = SyntaxState::new(Limits::permissive_default());
        for offset in 0..u32::from(u16::MAX) {
            step(&mut state, TokenKind::LBracket, offset);
        }
        assert_eq!(state.state().depth, u16::MAX);
        reject(&mut state, token(TokenKind::LBracket, u32::from(u16::MAX)), ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn nested_mixed_containers_count_once_and_restore_parent_phases() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.input.extend_from_slice(br#"{"x":[{},null]}"#);
        step(&mut state, TokenKind::LBrace, 0);
        accept(&mut state, raw_key(1, "x"));
        assert_eq!(state.frames.last().unwrap().count(), 1);
        step(&mut state, TokenKind::Colon, 4);
        step(&mut state, TokenKind::LBracket, 5);
        assert_eq!(state.frames.first().unwrap().phase(), FramePhase::ObjectCommaOrEnd);
        assert_eq!(state.frames.first().unwrap().count(), 1);
        step(&mut state, TokenKind::LBrace, 6);
        assert_eq!(state.frames.get(1).unwrap().count(), 1);
        assert_eq!(state.frames.last().unwrap().start_offset(), 6);
        assert_eq!(state.frames.last().unwrap().count(), 0);
        step(&mut state, TokenKind::RBrace, 7);
        assert_eq!(current_phase(&state), FramePhase::ArrayCommaOrEnd);
        step(&mut state, TokenKind::Comma, 8);
        assert_eq!(state.frames.last().unwrap().count(), 1);
        step(&mut state, TokenKind::Null, 9);
        assert_eq!(state.frames.last().unwrap().count(), 2);
        step(&mut state, TokenKind::RBracket, 13);
        assert_eq!(current_phase(&state), FramePhase::ObjectCommaOrEnd);
        assert_eq!(state.frames.last().unwrap().count(), 1);
        step(&mut state, TokenKind::RBrace, 14);
        assert_eq!(current_phase(&state), FramePhase::RootDone);
        assert!(state.frames.is_empty());
    }

    #[test]
    fn every_array_value_kind_counts_and_the_item_limit_is_inclusive() {
        for kind in [
            TokenKind::String, TokenKind::Number, TokenKind::True, TokenKind::False,
            TokenKind::Null, TokenKind::LBrace, TokenKind::LBracket,
        ] {
            let mut state = SyntaxState::new(Limits { max_array_items: 1, ..Limits::strict_default() });
            step(&mut state, TokenKind::LBracket, 0);
            step(&mut state, kind, 1);
            assert_eq!(state.frames.first().unwrap().count(), 1);
            match kind {
                TokenKind::LBrace => step(&mut state, TokenKind::RBrace, 2),
                TokenKind::LBracket => step(&mut state, TokenKind::RBracket, 2),
                _ => {}
            }
            step(&mut state, TokenKind::Comma, 10);
            reject(&mut state, token(kind, 11), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn object_field_limit_counts_keys_only_and_is_inclusive() {
        let mut state = SyntaxState::new(Limits {
            max_fields_per_object: 1, ..Limits::strict_default()
        });
        state.input.extend_from_slice(br#"{"a":0,"b":1}"#);
        step(&mut state, TokenKind::LBrace, 0);
        accept(&mut state, raw_key(1, "a"));
        assert_eq!(state.frames.last().unwrap().count(), 1);
        next_field(&mut state);
        assert_eq!(state.frames.last().unwrap().count(), 1);
        reject(&mut state, raw_key(7, "b"), ErrorCode::ConstraintViolation);
        assert_eq!(state.frames.last().unwrap().seen_keys.len(), 1);
    }

    #[test]
    fn counters_accept_u32_max_but_never_wrap() {
        for object in [false, true] {
            let mut state = SyntaxState::new(Limits::permissive_default());
            step(&mut state, if object { TokenKind::LBrace } else { TokenKind::LBracket }, 0);
            state.frames.last_mut().unwrap().count = u32::MAX - 1;
            let kind = if object { TokenKind::String } else { TokenKind::Number };
            step(&mut state, kind, 1);
            assert_eq!(state.frames.last().unwrap().count(), u32::MAX);
            if object {
                next_field(&mut state);
            } else {
                step(&mut state, TokenKind::Comma, 3);
            }
            reject(&mut state, token(kind, 4), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn raw_and_escaped_keys_compare_by_decoded_bytes_in_either_order() {
        for escaped_first in [false, true] {
            let mut state = SyntaxState::new(Limits::strict_default());
            state.input.extend_from_slice(br#"{"a":0,"\u0061":1}"#);
            state.decoded.extend_from_slice(b"a");
            let raw = raw_key(1, "a");
            let escaped = string_token(7, 8, StrRef {
                span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1,
            });
            let (first, second) = if escaped_first { (escaped, raw) } else { (raw, escaped) };
            step(&mut state, TokenKind::LBrace, 0);
            accept(&mut state, first);
            assert_eq!(state.frames.last().unwrap().seen_keys[0].as_slice(), b"a");
            next_field(&mut state);
            reject(&mut state, second, ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn empty_and_embedded_nul_keys_are_duplicate_checked() {
        for text in ["", "\0", "\u{1f600}"] {
            let mut state = SyntaxState::new(Limits::strict_default());
            state.decoded.extend_from_slice(text.as_bytes());
            let key = string_token(1, 14, StrRef {
                span: Span32::new(0, u32::try_from(text.len()).unwrap()),
                source: SpanSource::DecodedArena,
                scalar_len: u32::try_from(text.chars().count()).unwrap(),
            });
            step(&mut state, TokenKind::LBrace, 0);
            accept(&mut state, key);
            next_field(&mut state);
            reject(&mut state, key, ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn key_comparison_does_not_normalize_fold_case_or_match_prefixes() {
        let mut state = SyntaxState::new(Limits::strict_default());
        step(&mut state, TokenKind::LBrace, 0);
        let texts = ["a", "A", "ab", "\u{e9}", "e\u{301}", "a\0", "a\0b"];
        for (index, text) in texts.into_iter().enumerate() {
            if index != 0 {
                next_field(&mut state);
            }
            let start = u32::try_from(state.decoded.len()).unwrap();
            state.decoded.extend_from_slice(text.as_bytes());
            accept(&mut state, string_token(1, 32, StrRef {
                span: Span32::new(start, u32::try_from(text.len()).unwrap()),
                source: SpanSource::DecodedArena,
                scalar_len: u32::try_from(text.chars().count()).unwrap(),
            }));
        }
        let frame = state.frames.last().unwrap();
        assert_eq!(frame.count(), 7);
        assert_eq!(frame.seen_keys.len(), 7);
    }

    #[test]
    fn duplicate_policy_off_keeps_no_owned_key_storage() {
        let mut state = SyntaxState::new(Limits {
            reject_duplicate_keys: false, ..Limits::strict_default()
        });
        state.input.extend_from_slice(br#"{"a":0}"#);
        step(&mut state, TokenKind::LBrace, 0);
        for index in 0..3 {
            if index != 0 {
                next_field(&mut state);
            }
            accept(&mut state, raw_key(1, "a"));
        }
        let frame = state.frames.last().unwrap();
        assert_eq!(frame.count(), 3);
        assert!(frame.seen_keys.is_empty());
        assert_eq!(frame.seen_keys.capacity(), 0);
    }

    #[test]
    fn unknown_key_policy_has_no_generic_syntax_effect() {
        for reject_unknown_keys in [false, true] {
            let mut state = SyntaxState::new(Limits {
                reject_unknown_keys, ..Limits::strict_default()
            });
            state.input.extend_from_slice(br#"{"unknown":0}"#);
            step(&mut state, TokenKind::LBrace, 0);
            accept(&mut state, raw_key(1, "unknown"));
            step(&mut state, TokenKind::Colon, 10);
            step(&mut state, TokenKind::Number, 11);
            step(&mut state, TokenKind::RBrace, 12);
            assert_eq!(state.limits().reject_unknown_keys, reject_unknown_keys);
            assert!(state.failure().is_none());
        }
    }

    #[test]
    fn key_sets_are_scoped_to_each_object_and_survive_child_pop() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.input.extend_from_slice(br#"{"a":{"a":0},"a":1}"#);
        step(&mut state, TokenKind::LBrace, 0);
        accept(&mut state, raw_key(1, "a"));
        step(&mut state, TokenKind::Colon, 4);
        step(&mut state, TokenKind::LBrace, 5);
        accept(&mut state, raw_key(6, "a"));
        step(&mut state, TokenKind::Colon, 9);
        step(&mut state, TokenKind::Number, 10);
        step(&mut state, TokenKind::RBrace, 11);
        assert_eq!(state.frames.last().unwrap().seen_keys.len(), 1);
        step(&mut state, TokenKind::Comma, 12);
        reject(&mut state, raw_key(13, "a"), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn sibling_objects_have_independent_key_sets() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.input.extend_from_slice(br#"[{"a":0},{"a":0}]"#);
        step(&mut state, TokenKind::LBracket, 0);
        for start in [1, 9] {
            if start != 1 {
                step(&mut state, TokenKind::Comma, 8);
            }
            step(&mut state, TokenKind::LBrace, start);
            accept(&mut state, raw_key(start + 1, "a"));
            step(&mut state, TokenKind::Colon, start + 4);
            step(&mut state, TokenKind::Number, start + 5);
            step(&mut state, TokenKind::RBrace, start + 6);
        }
        assert_eq!(state.frames.last().unwrap().count(), 2);
        assert!(state.frames.last().unwrap().seen_keys.is_empty());
        step(&mut state, TokenKind::RBracket, 16);
    }

    #[test]
    fn owned_keys_survive_backing_storage_growth() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.decoded.extend_from_slice(b"a");
        let key = string_token(1, 8, StrRef {
            span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1,
        });
        step(&mut state, TokenKind::LBrace, 0);
        accept(&mut state, key);
        state.decoded.resize(4096, b'x');
        state.input.resize(4096, b' ');
        next_field(&mut state);
        reject(&mut state, key, ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn invalid_key_owners_and_utf8_fail_without_committing_a_key() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for bytes in [&b""[..], &b"\xff"[..]] {
                let mut state = SyntaxState::new(Limits::strict_default());
                let reference = match source {
                    SpanSource::RawPayload => {
                        state.input.extend_from_slice(b"{\"");
                        state.input.extend_from_slice(bytes);
                        StrRef { span: Span32::new(2, 1), source, scalar_len: 1 }
                    }
                    SpanSource::DecodedArena => {
                        state.decoded.extend_from_slice(bytes);
                        StrRef { span: Span32::new(0, 1), source, scalar_len: 1 }
                    }
                };
                step(&mut state, TokenKind::LBrace, 0);
                reject(&mut state, string_token(1, 3, reference), ErrorCode::InternalError);
            }
        }
    }

    #[test]
    fn mismatched_and_stale_transitions_are_internal_errors() {
        let value = token(TokenKind::LBracket, 19);
        let valid = machine::transition(FramePhase::RootStart, value).unwrap();
        for wrong in [
            Transition { action: FrameAction::Pop, ..valid },
            Transition { action: FrameAction::PushObject, ..valid },
            Transition { next_phase: FramePhase::ArrayCommaOrEnd, ..valid },
            Transition { event: None, ..valid },
            Transition { event: Some(Event::ArrayStart { offset: 20 }), ..valid },
        ] {
            let mut state = SyntaxState::new(Limits::strict_default());
            reject_transition(&mut state, value, wrong, ErrorCode::InternalError);
        }
        let mut state = SyntaxState::new(Limits::strict_default());
        step(&mut state, TokenKind::LBracket, 0);
        reject_transition(&mut state, value, valid, ErrorCode::InternalError);
    }

    #[test]
    fn unmatched_close_and_wrong_frame_kind_cannot_pop() {
        let close = token(TokenKind::RBrace, 33);
        let transition = machine::transition(FramePhase::ObjectStart, close).unwrap();
        let mut empty = SyntaxState::new(Limits::strict_default());
        reject_transition(&mut empty, close, transition, ErrorCode::InternalError);
        let mut array = SyntaxState::new(Limits::strict_default());
        step(&mut array, TokenKind::LBracket, 0);
        reject_transition(&mut array, close, transition, ErrorCode::InternalError);
        let mut corrupt = SyntaxState::new(Limits::strict_default());
        step(&mut corrupt, TokenKind::LBracket, 0);
        corrupt.frames.last_mut().unwrap().phase = FramePhase::ObjectStart;
        reject_transition(&mut corrupt, close, transition, ErrorCode::InternalError);
    }

    #[test]
    fn corrupt_root_phase_and_unadvanced_root_are_internal_errors() {
        let value = token(TokenKind::Null, 9);
        let transition = machine::transition(FramePhase::ArrayStart, value).unwrap();
        let mut state = SyntaxState::new(Limits::strict_default());
        state.root_phase = FramePhase::ArrayStart;
        reject_transition(&mut state, value, transition, ErrorCode::InternalError);
        let mut state = SyntaxState::new(Limits::strict_default());
        step(&mut state, TokenKind::LBracket, 0);
        state.root_phase = FramePhase::RootStart;
        reject_transition(&mut state, value, transition, ErrorCode::InternalError);
    }

    #[test]
    fn preexisting_failure_wins_over_all_further_operations() {
        let mut state = SyntaxState::new(Limits::strict_default());
        let first = Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8, 7, "invalid UTF-8")
            .with_schema(21, 34);
        state.fail(first.clone());
        let before = snapshot(&state);
        let token = token(TokenKind::LBrace, 100);
        let transition = Transition {
            next_phase: FramePhase::RootDone, action: FrameAction::Pop, event: None,
        };
        assert_eq!(apply(&mut state, token, transition), Err(first.clone()));
        assert_eq!(state.failure(), Some(&first));
        assert_eq!(snapshot(&state), before);
    }

    #[test]
    fn framing_leaves_feed_owned_state_untouched() {
        let mut state = SyntaxState::new(Limits::strict_default());
        state.input.extend_from_slice(b"[]");
        state.decoded.extend_from_slice(b"decoded");
        state.byte_offset = 1;
        state.final_input = true;
        state.pending = Some(PendingToken {
            kind: PendingKind::String, start: 1, scan_offset: 2, escaped: true,
        });
        let before = snapshot(&state);
        step(&mut state, TokenKind::LBracket, 0);
        step(&mut state, TokenKind::RBracket, 1);
        let after = snapshot(&state);
        assert_eq!(after.input, before.input);
        assert_eq!(after.decoded, before.decoded);
        assert_eq!(after.offset, before.offset);
        assert_eq!(after.pending, before.pending);
        assert_eq!(after.final_input, before.final_input);
        assert_eq!(after.finished, before.finished);
        assert_eq!(after.limits, before.limits);
    }

    #[test]
    fn reservation_capacity_overflow_maps_to_budget_exhausted() {
        // A capacity overflow is deterministic and does not exhaust host memory.
        // The same helper serves frame, key-list, and owned-key reservations.
        let mut bytes = Vec::new();
        bytes.push(42u8);
        let capacity = bytes.capacity();
        assert_error(
            &reserve(&mut bytes, usize::MAX, 71).unwrap_err(),
            ErrorCode::BudgetExhausted, 71,
        );
        assert_eq!(bytes.as_slice(), &[42]);
        assert_eq!(bytes.capacity(), capacity);
        let mut frames: Vec<Frame> = Vec::new();
        assert_error(
            &reserve(&mut frames, usize::MAX, 72).unwrap_err(),
            ErrorCode::BudgetExhausted, 72,
        );
        assert!(frames.is_empty());
        let mut keys: Vec<Vec<u8>> = Vec::new();
        assert_error(
            &reserve(&mut keys, usize::MAX, 73).unwrap_err(),
            ErrorCode::BudgetExhausted, 73,
        );
        assert!(keys.is_empty());
        reserve(&mut bytes, 0, 74).unwrap();
        reserve(&mut keys, 1, 75).unwrap();
        assert!(keys.capacity() >= 1);
    }
}
