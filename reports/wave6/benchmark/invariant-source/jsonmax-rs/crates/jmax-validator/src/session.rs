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
