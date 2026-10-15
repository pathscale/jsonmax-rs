//! Explicit, host-driven reactor client state for validator requests.
//!
//! This module owns only lifecycle and request bookkeeping. A `SessionReactor`
//! remains owned by the embedding host and is called synchronously; none of the
//! functions below waits for connection progress or starts background work.

use super::*;

fn state_error(session: &Session, detail: &'static str) -> ValidatorError {
    ValidatorError::State { phase: session.phase, detail }
}

fn validation_error(code: ErrorCode, detail: &'static str) -> ValidatorError {
    ValidatorError::Validation(AbiError::new(code, ErrorSource::Decode, 0, detail))
}

fn framing_error(detail: &'static str) -> ValidatorError {
    validation_error(ErrorCode::FramingError, detail)
}

fn fail(session: &mut Session) {
    session.phase = SessionPhase::Failed;
    session.pending.clear();
}

/// Creates a disconnected session after validating all required budgets.
pub fn new_session(config: SessionConfig) -> ValidatorResult<Session> {
    if config.max_pending == 0
        || config.budget.cpu_units == 0
        || config.budget.retained_bytes == 0
        || config.budget.timeout_ms == 0
    {
        return Err(validation_error(
            ErrorCode::BudgetExhausted,
            "session limits and resource budgets must be nonzero",
        ));
    }
    Ok(Session {
        phase: SessionPhase::Disconnected,
        next_sequence: 1,
        pending: BTreeMap::new(),
        config,
    })
}

/// Starts connection establishment exactly once. Progress is reported by poll.
pub fn connect(session: &mut Session, reactor: &mut dyn SessionReactor) -> ValidatorResult<()> {
    if session.phase != SessionPhase::Disconnected {
        return Err(state_error(session, "connect requires a disconnected session"));
    }
    match reactor.connect() {
        Ok(()) => {
            session.phase = SessionPhase::Connecting;
            Ok(())
        }
        Err(error) => {
            fail(session);
            Err(error)
        }
    }
}

/// Validates and sends a prepared parameter payload, recording it only after
/// the reactor confirms atomic acceptance.
pub fn submit(
    session: &mut Session,
    model: &ValidatorModel,
    params: PreparedParams,
    reactor: &mut dyn SessionReactor,
    limits: Limits,
) -> ValidatorResult<u64> {
    if session.phase != SessionPhase::Ready {
        return Err(state_error(session, "submit requires a ready session"));
    }

    let endpoint = model.endpoint(params.endpoint).ok_or_else(|| {
        validation_error(ErrorCode::MissingRequiredField, "selected endpoint is missing or ambiguous")
    })?;
    super::params::validate_prepared(model, &params, limits)?;

    if session.pending.len() >= session.config.max_pending as usize
        || params.payload.len() > session.config.budget.retained_bytes as usize
    {
        return Err(validation_error(
            ErrorCode::BudgetExhausted,
            "pending request or retained payload budget exhausted",
        ));
    }

    let sequence = session.next_sequence;
    let next_sequence = sequence.checked_add(1).ok_or_else(|| {
        validation_error(ErrorCode::NumberOverflow, "request sequence space exhausted")
    })?;

    let streaming = endpoint.stream_response.is_some();
    reactor.send(sequence, params.clone(), session.config.budget)?;

    session.pending.insert(sequence, PendingRequest { sequence, endpoint: params.endpoint, streaming });
    session.next_sequence = next_sequence;
    Ok(sequence)
}

/// Consumes and validates no more than one event from the host reactor.
/// Payload schema validation is a `SessionReactor` responsibility: it must
/// validate response bytes for the associated endpoint before constructing an
/// event, because this operation intentionally does not borrow a model.
pub fn poll(
    session: &mut Session,
    reactor: &mut dyn SessionReactor,
) -> ValidatorResult<Option<SessionEvent>> {
    if !matches!(session.phase, SessionPhase::Connecting | SessionPhase::Ready | SessionPhase::Closing) {
        return Err(state_error(session, "poll requires a connecting, ready, or closing session"));
    }

    let event = match reactor.poll_event() {
        Ok(event) => event,
        Err(error) => {
            fail(session);
            return Err(error);
        }
    };
    let Some(event) = event else { return Ok(None) };

    match &event {
        SessionEvent::Connected => {
            if session.phase != SessionPhase::Connecting {
                return Err(state_error(session, "connected event is valid only while connecting"));
            }
            session.phase = SessionPhase::Ready;
        }
        SessionEvent::Closed => {
            if !matches!(session.phase, SessionPhase::Connecting | SessionPhase::Ready | SessionPhase::Closing) {
                return Err(state_error(session, "closed event is not valid in this session phase"));
            }
            session.phase = SessionPhase::Closed;
            session.pending.clear();
        }
        SessionEvent::Immediate { sequence, .. } => {
            if !matches!(session.phase, SessionPhase::Ready | SessionPhase::Closing) {
                fail(session);
                return Err(framing_error("immediate response arrived in an invalid phase"));
            }
            let Some(request) = session.pending.get(sequence) else {
                fail(session);
                return Err(framing_error("response references an unknown request sequence"));
            };
            if request.sequence != *sequence {
                fail(session);
                return Err(framing_error("pending request sequence is inconsistent"));
            }
            if !request.streaming {
                session.pending.remove(sequence);
            }
        }
        SessionEvent::Stream { sequence, .. } => {
            if !matches!(session.phase, SessionPhase::Ready | SessionPhase::Closing) {
                fail(session);
                return Err(framing_error("stream event arrived in an invalid phase"));
            }
            match session.pending.get(sequence) {
                Some(request) if request.sequence == *sequence && request.streaming => {}
                Some(_) => {
                    fail(session);
                    return Err(framing_error("stream event references a nonstreaming request"));
                }
                None => {
                    fail(session);
                    return Err(framing_error("stream event references an unknown request sequence"));
                }
            }
        }
        SessionEvent::RemoteError { sequence, .. } => {
            if !matches!(session.phase, SessionPhase::Ready | SessionPhase::Closing) {
                fail(session);
                return Err(framing_error("remote error arrived in an invalid phase"));
            }
            match session.pending.get(sequence) {
                Some(request) if request.sequence == *sequence => { session.pending.remove(sequence); }
                _ => {
                    fail(session);
                    return Err(framing_error("remote error references an unknown request sequence"));
                }
            }
        }
        SessionEvent::StreamClosed { sequence } => {
            if !matches!(session.phase, SessionPhase::Ready | SessionPhase::Closing) {
                fail(session);
                return Err(framing_error("stream close arrived in an invalid phase"));
            }
            match session.pending.get(sequence) {
                Some(request) if request.sequence == *sequence && request.streaming => {
                    session.pending.remove(sequence);
                }
                Some(_) => {
                    fail(session);
                    return Err(framing_error("stream close references a nonstreaming request"));
                }
                None => {
                    fail(session);
                    return Err(framing_error("stream close references an unknown request sequence"));
                }
            }
        }
    }
    Ok(Some(event))
}

/// Begins graceful closure, or performs one cleanup call for a failed session.
pub fn close(session: &mut Session, reactor: &mut dyn SessionReactor) -> ValidatorResult<()> {
    match session.phase {
        SessionPhase::Disconnected => {
            session.phase = SessionPhase::Closed;
            session.pending.clear();
            Ok(())
        }
        SessionPhase::Connecting | SessionPhase::Ready => match reactor.close() {
            Ok(()) => {
                session.phase = SessionPhase::Closing;
                Ok(())
            }
            Err(error) => {
                fail(session);
                Err(error)
            }
        },
        SessionPhase::Closing | SessionPhase::Closed => Ok(()),
        SessionPhase::Failed => match reactor.close() {
            Ok(()) => {
                session.phase = SessionPhase::Closed;
                session.pending.clear();
                Ok(())
            }
            Err(error) => {
                fail(session);
                Err(error)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct MockReactor {
        connect_calls: usize,
        send_calls: Vec<(u64, Vec<u8>, ResourceBudget)>,
        poll_calls: usize,
        close_calls: usize,
        events: VecDeque<SessionEvent>,
        connect_error: bool,
        send_error: bool,
        poll_error: bool,
        close_error: bool,
    }

    impl SessionReactor for MockReactor {
        fn connect(&mut self) -> ValidatorResult<()> {
            self.connect_calls += 1;
            if self.connect_error { Err(test_state("connect failed")) } else { Ok(()) }
        }

        fn send(&mut self, sequence: u64, params: PreparedParams, budget: ResourceBudget) -> ValidatorResult<()> {
            self.send_calls.push((sequence, params.payload.as_slice().to_vec(), budget));
            if self.send_error { Err(test_state("send failed")) } else { Ok(()) }
        }

        fn poll_event(&mut self) -> ValidatorResult<Option<SessionEvent>> {
            self.poll_calls += 1;
            if self.poll_error { return Err(test_state("poll failed")); }
            Ok(self.events.pop_front())
        }

        fn close(&mut self) -> ValidatorResult<()> {
            self.close_calls += 1;
            if self.close_error { Err(test_state("close failed")) } else { Ok(()) }
        }
    }

    fn test_state(detail: &'static str) -> ValidatorError {
        ValidatorError::State { phase: SessionPhase::Failed, detail }
    }

    fn config(max_pending: u32, retained_bytes: u32) -> SessionConfig {
        SessionConfig {
            max_pending,
            budget: ResourceBudget { cpu_units: 10, retained_bytes, timeout_ms: 20 },
        }
    }

    fn model(streaming: bool) -> (ValidatorModel, EndpointKey) {
        let key = EndpointKey { service_id: 3, code: 9 };
        let endpoint = EndpointSchemaSpec {
            code: key.code,
            name: String::from("request"),
            service: String::from("svc"),
            roles: Vec::new(),
            params: Vec::new(),
            response: Vec::new(),
            stream_response: streaming.then_some(EndpointType::String),
            description: String::new(),
        };
        (ValidatorModel {
            services: vec![ServiceSpec { id: key.service_id, name: String::from("svc"), endpoints: vec![endpoint] }],
            presets: Vec::new(),
        }, key)
    }

    fn prepared(key: EndpointKey, bytes: &[u8]) -> PreparedParams {
        PreparedParams { endpoint: key, payload: Payload::from_slice(bytes) }
    }

    fn ready_session(max_pending: u32, retained_bytes: u32, reactor: &mut MockReactor) -> Session {
        let mut session = new_session(config(max_pending, retained_bytes)).unwrap();
        connect(&mut session, reactor).unwrap();
        reactor.events.push_back(SessionEvent::Connected);
        assert!(matches!(poll(&mut session, reactor).unwrap(), Some(SessionEvent::Connected)));
        session
    }

    fn code<T>(result: ValidatorResult<T>) -> ErrorCode {
        match result.unwrap_err() {
            ValidatorError::Validation(error) => error.code,
            other => panic!("expected validation error, got {other}"),
        }
    }

    #[test]
    fn creates_disconnected_session_with_sequence_one_and_empty_pending() {
        let session = new_session(config(1, 1)).unwrap();
        assert_eq!(session.phase(), SessionPhase::Disconnected);
        assert_eq!(session.next_sequence, 1);
        assert!(session.pending().is_empty());
        assert_eq!(session.config(), config(1, 1));
    }

    #[test]
    fn rejects_each_zero_capacity_or_budget_field() {
        for candidate in [
            SessionConfig { max_pending: 0, ..config(1, 1) },
            SessionConfig { budget: ResourceBudget { cpu_units: 0, ..config(1, 1).budget }, ..config(1, 1) },
            SessionConfig { budget: ResourceBudget { retained_bytes: 0, ..config(1, 1).budget }, ..config(1, 1) },
            SessionConfig { budget: ResourceBudget { timeout_ms: 0, ..config(1, 1).budget }, ..config(1, 1) },
        ] {
            assert_eq!(code(new_session(candidate)), ErrorCode::BudgetExhausted);
        }
        assert!(new_session(config(u32::MAX, u32::MAX)).is_ok());
    }

    #[test]
    fn connect_is_single_transition_and_reactor_failure_fails_session() {
        let mut reactor = MockReactor::default();
        let mut session = new_session(config(1, 10)).unwrap();
        assert!(connect(&mut session, &mut reactor).is_ok());
        assert_eq!(session.phase(), SessionPhase::Connecting);
        assert_eq!(reactor.connect_calls, 1);
        assert!(matches!(connect(&mut session, &mut reactor), Err(ValidatorError::State { .. })));
        assert_eq!(reactor.connect_calls, 1);

        let mut failed_reactor = MockReactor { connect_error: true, ..MockReactor::default() };
        let mut failed = new_session(config(1, 10)).unwrap();
        assert!(connect(&mut failed, &mut failed_reactor).is_err());
        assert_eq!(failed.phase(), SessionPhase::Failed);
    }

    #[test]
    fn poll_none_preserves_state_and_connected_event_makes_ready() {
        let mut reactor = MockReactor::default();
        let mut session = new_session(config(2, 20)).unwrap();
        connect(&mut session, &mut reactor).unwrap();
        assert!(poll(&mut session, &mut reactor).unwrap().is_none());
        assert_eq!(session.phase(), SessionPhase::Connecting);
        reactor.events.push_back(SessionEvent::Connected);
        assert!(matches!(poll(&mut session, &mut reactor).unwrap(), Some(SessionEvent::Connected)));
        assert_eq!(session.phase(), SessionPhase::Ready);
        assert_eq!(reactor.poll_calls, 2);
    }

    #[test]
    fn submission_tracks_sequence_and_validates_payload_before_atomic_send() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(2, 2, &mut reactor);
        assert_eq!(submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap(), 1);
        assert_eq!(session.next_sequence, 2);
        assert_eq!(session.pending().get(&1).unwrap().endpoint, key);
        assert!(!session.pending().get(&1).unwrap().streaming);
        assert_eq!(reactor.send_calls[0].0, 1);
        assert_eq!(reactor.send_calls[0].1, b"{}");

        assert_eq!(code(submit(&mut session, &model, prepared(key, b"{bad}"), &mut reactor, Limits::strict_default())), ErrorCode::InvalidSyntax);
        assert_eq!(session.next_sequence, 2);
        assert_eq!(session.pending().len(), 1);
        assert_eq!(reactor.send_calls.len(), 1);
    }

    #[test]
    fn capacity_and_retained_byte_boundaries_do_not_mutate_request_state() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(1, 2, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        assert_eq!(code(submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default())), ErrorCode::BudgetExhausted);
        assert_eq!(session.pending().len(), 1);
        assert_eq!(session.next_sequence, 2);
        assert_eq!(reactor.send_calls.len(), 1);

        let mut bytes_reactor = MockReactor::default();
        let mut bytes_session = ready_session(1, 1, &mut bytes_reactor);
        assert_eq!(code(submit(&mut bytes_session, &model, prepared(key, b"{}"), &mut bytes_reactor, Limits::strict_default())), ErrorCode::BudgetExhausted);
        assert!(bytes_session.pending().is_empty());
        assert_eq!(bytes_session.next_sequence, 1);
        assert!(bytes_reactor.send_calls.is_empty());
    }

    #[test]
    fn send_failure_keeps_sequence_and_pending_unchanged() {
        let (model, key) = model(false);
        let mut reactor = MockReactor { send_error: true, ..MockReactor::default() };
        let mut session = ready_session(1, 10, &mut reactor);
        assert!(submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).is_err());
        assert_eq!(session.next_sequence, 1);
        assert!(session.pending().is_empty());
        assert_eq!(reactor.send_calls.len(), 1);
    }

    #[test]
    fn immediate_and_remote_error_complete_unary_requests() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(2, 10, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        reactor.events.push_back(SessionEvent::Immediate { sequence: 1, payload: Payload::from_slice(b"{}") });
        assert!(matches!(poll(&mut session, &mut reactor).unwrap(), Some(SessionEvent::Immediate { sequence: 1, .. })));
        assert!(session.pending().is_empty());

        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        reactor.events.push_back(SessionEvent::RemoteError { sequence: 2, payload: Payload::from_slice(b"error") });
        assert!(matches!(poll(&mut session, &mut reactor).unwrap(), Some(SessionEvent::RemoteError { sequence: 2, .. })));
        assert!(session.pending().is_empty());
    }

    #[test]
    fn streams_remain_pending_until_stream_closed_and_immediate_does_not_finish_them() {
        let (model, key) = model(true);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(2, 10, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        assert!(session.pending().get(&1).unwrap().streaming);
        reactor.events.push_back(SessionEvent::Immediate { sequence: 1, payload: Payload::from_slice(b"{}") });
        poll(&mut session, &mut reactor).unwrap();
        reactor.events.push_back(SessionEvent::Stream { sequence: 1, payload: Payload::from_slice(b"\"tick\"") });
        assert!(matches!(poll(&mut session, &mut reactor).unwrap(), Some(SessionEvent::Stream { sequence: 1, .. })));
        assert_eq!(session.pending().len(), 1);
        reactor.events.push_back(SessionEvent::StreamClosed { sequence: 1 });
        poll(&mut session, &mut reactor).unwrap();
        assert!(session.pending().is_empty());
    }

    #[test]
    fn unknown_sequence_or_wrong_event_kind_fails_and_clears_pending() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(2, 10, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        reactor.events.push_back(SessionEvent::Stream { sequence: 1, payload: Payload::from_slice(b"null") });
        assert_eq!(code(poll(&mut session, &mut reactor)), ErrorCode::FramingError);
        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(session.pending().is_empty());
    }

    #[test]
    fn reactor_poll_error_fails_session_and_closed_event_clears_requests() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(2, 10, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        reactor.events.push_back(SessionEvent::Closed);
        poll(&mut session, &mut reactor).unwrap();
        assert_eq!(session.phase(), SessionPhase::Closed);
        assert!(session.pending().is_empty());

        let mut poll_failed_reactor = MockReactor { poll_error: true, ..MockReactor::default() };
        let mut poll_failed = ready_session(2, 10, &mut poll_failed_reactor);
        assert!(poll(&mut poll_failed, &mut poll_failed_reactor).is_err());
        assert_eq!(poll_failed.phase(), SessionPhase::Failed);
    }

    #[test]
    fn close_handles_disconnected_closing_closed_and_failed_states() {
        let mut reactor = MockReactor::default();
        let mut disconnected = new_session(config(1, 10)).unwrap();
        close(&mut disconnected, &mut reactor).unwrap();
        assert_eq!(disconnected.phase(), SessionPhase::Closed);
        assert_eq!(reactor.close_calls, 0);

        let mut active_reactor = MockReactor::default();
        let mut active = ready_session(1, 10, &mut active_reactor);
        close(&mut active, &mut active_reactor).unwrap();
        assert_eq!(active.phase(), SessionPhase::Closing);
        assert_eq!(active_reactor.close_calls, 1);
        close(&mut active, &mut active_reactor).unwrap();
        assert_eq!(active_reactor.close_calls, 1);
        active_reactor.events.push_back(SessionEvent::Closed);
        poll(&mut active, &mut active_reactor).unwrap();
        close(&mut active, &mut active_reactor).unwrap();
        assert_eq!(active.phase(), SessionPhase::Closed);
        assert_eq!(active_reactor.close_calls, 1);

        let mut failed_reactor = MockReactor::default();
        let mut failed = new_session(config(1, 10)).unwrap();
        failed.phase = SessionPhase::Failed;
        close(&mut failed, &mut failed_reactor).unwrap();
        assert_eq!(failed.phase(), SessionPhase::Closed);
        assert_eq!(failed_reactor.close_calls, 1);
    }

    #[test]
    fn close_failure_transitions_to_failed_and_clears_pending() {
        let (model, key) = model(false);
        let mut reactor = MockReactor { close_error: true, ..MockReactor::default() };
        let mut session = ready_session(1, 10, &mut reactor);
        submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()).unwrap();
        assert!(close(&mut session, &mut reactor).is_err());
        assert_eq!(session.phase(), SessionPhase::Failed);
        assert!(session.pending().is_empty());
    }

    #[test]
    fn sequence_increment_boundary_rejects_without_sending_or_reusing_id() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut session = ready_session(1, 10, &mut reactor);
        session.next_sequence = u64::MAX;
        assert_eq!(code(submit(&mut session, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default())), ErrorCode::NumberOverflow);
        assert_eq!(session.next_sequence, u64::MAX);
        assert!(session.pending().is_empty());
        assert!(reactor.send_calls.is_empty());
    }

    #[test]
    fn submit_requires_ready_state_and_unambiguous_endpoint() {
        let (model, key) = model(false);
        let mut reactor = MockReactor::default();
        let mut disconnected = new_session(config(1, 10)).unwrap();
        assert!(matches!(submit(&mut disconnected, &model, prepared(key, b"{}"), &mut reactor, Limits::strict_default()), Err(ValidatorError::State { .. })));

        let missing = PreparedParams { endpoint: EndpointKey { service_id: 99, code: 99 }, payload: Payload::from_slice(b"{}") };
        let mut session = ready_session(1, 10, &mut reactor);
        assert_eq!(code(submit(&mut session, &model, missing, &mut reactor, Limits::strict_default())), ErrorCode::MissingRequiredField);
        assert!(session.pending().is_empty());
        assert!(reactor.send_calls.is_empty());
    }

    #[test]
    fn duplicate_connected_event_is_a_state_error_without_reopening() {
        let mut reactor = MockReactor::default();
        let mut session = ready_session(1, 10, &mut reactor);
        reactor.events.push_back(SessionEvent::Connected);
        assert!(matches!(poll(&mut session, &mut reactor), Err(ValidatorError::State { .. })));
        assert_eq!(session.phase(), SessionPhase::Ready);
    }
}
