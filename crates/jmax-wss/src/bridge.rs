//! Owned CPU-dispatch boundaries and explicit, nonblocking local delivery.
//!
//! Endpoint and MCP routing, full request accounting, and scheduling belong to
//! the configured dispatcher. The bridge never obtains a reactor or executor.
//! Only owned requests and completions cross that seam; the transport remains
//! inside its reactor-local binding.

use super::*;

/// Transfers one complete protected message to the configured dispatcher.
///
/// Admission requires nonzero CPU and timeout allowances and enough retained
/// bytes for the input. The dispatcher must additionally account for decoded
/// storage, scratch, and output within the unchanged request budget. Protection
/// establishes transport completeness, so JSON and schema validation still
/// belong to the dispatcher. Dispatcher errors propagate without retrying.
pub fn submit_input(
    dispatcher: &mut dyn OwnedDispatcher,
    connection: ConnectionId,
    input: ProtectedInput,
    budget: ResourceBudget,
) -> Result<()> {
    ensure_request_budget(input.payload().len(), budget)?;
    dispatcher.submit(BridgeRequest { connection, input, budget })
}

/// Polls once and transfers at most one validated output to its connection.
///
/// Returns `false` only when the poll has no completion. Poll, completion, and
/// transport failures propagate unchanged. A consumed completion is never
/// retried, including when the transport reports backpressure. The host must
/// provide retry storage if needed. `try_send` must enqueue the entire immutable
/// frame atomically or fail without publishing a partial frame.
///
/// The transport must reject completions for closed connections. Connection IDs
/// must not be reused while old work can complete; the bridge has no connection
/// registry and cannot distinguish different lifetimes sharing the same ID.
pub fn forward_one<T: LocalTransport>(
    binding: &mut ReactorBinding<T>,
    dispatcher: &mut dyn OwnedDispatcher,
) -> Result<bool> {
    let completion = dispatcher.poll_completion()?.map(|completion| {
        (completion.connection, completion.output)
    });
    deliver_completion(completion, |connection, output| {
        binding.transport_mut().try_send(connection, output)
    })
}

/// Validates and requests one nonblocking transport close.
///
/// The reason is already valid UTF-8 through its `str` type and may contain at
/// most 123 bytes, leaving two bytes for the status code. It need not be JSON.
/// Invalid arguments never reach the transport. The transport owns connection
/// lifecycle bookkeeping and reports any close failure unchanged.
pub fn close_connection<T: LocalTransport>(
    binding: &mut ReactorBinding<T>,
    connection: ConnectionId,
    code: u16,
    reason: &str,
) -> Result<()> {
    if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
        return Err(transport_error(ErrorCode::FramingError, "invalid WebSocket close code"));
    }
    if reason.len() > MAX_CONTROL_BYTES - 2 {
        return Err(transport_error(
            ErrorCode::PayloadTooLarge,
            "WebSocket close reason exceeds 123 bytes",
        ));
    }
    binding.transport_mut().close(connection, code, reason)
}

fn ensure_request_budget(payload_len: usize, budget: ResourceBudget) -> Result<()> {
    if budget.cpu_units == 0
        || budget.timeout_ms == 0
        || u32::try_from(payload_len).map_or(true, |len| len > budget.retained_bytes)
    {
        return Err(transport_error(
            ErrorCode::BudgetExhausted,
            "WebSocket request exceeds CPU, timeout, or retained byte budget",
        ));
    }
    Ok(())
}

// Ownership delivery does not inspect, clone, or manufacture its output. The
// public entry point specializes this helper exclusively to ValidatedOutput.
fn deliver_completion<O>(
    completion: Option<(ConnectionId, Result<O>)>,
    send: impl FnOnce(ConnectionId, O) -> Result<()>,
) -> Result<bool> {
    let Some((connection, output)) = completion else {
        return Ok(false);
    };
    send(connection, output?)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::VecDeque, rc::Rc, string::String, vec::Vec};
    use core::cell::{Cell, RefCell};

    #[derive(Default)]
    struct RecordingDispatcher {
        submit_calls: usize,
        poll_calls: usize,
        requests: Vec<BridgeRequest>,
        submit_error: Option<Error>,
        polls: VecDeque<Result<Option<BridgeCompletion>>>,
    }

    impl OwnedDispatcher for RecordingDispatcher {
        fn submit(&mut self, request: BridgeRequest) -> Result<()> {
            self.submit_calls += 1;
            if let Some(error) = &self.submit_error {
                return Err(error.clone());
            }
            self.requests.push(request);
            Ok(())
        }

        fn poll_completion(&mut self) -> Result<Option<BridgeCompletion>> {
            self.poll_calls += 1;
            self.polls.pop_front().unwrap_or(Ok(None))
        }
    }

    #[derive(Default)]
    struct TransportLog {
        send_calls: usize,
        close_calls: usize,
        sent: Vec<(ConnectionId, ValidatedOutput)>,
        closes: Vec<(ConnectionId, u16, String)>,
        closed: Vec<ConnectionId>,
        send_error: Option<Error>,
        close_error: Option<Error>,
    }

    // Rc intentionally permits a local-only implementation, ensuring the
    // bridge never imposes Send or Sync bounds on the reactor or transport.
    struct RecordingTransport(Rc<RefCell<TransportLog>>);

    impl LocalTransport for RecordingTransport {
        fn try_send(&mut self, connection: ConnectionId, output: ValidatedOutput) -> Result<()> {
            let mut log = self.0.borrow_mut();
            log.send_calls += 1;
            if log.closed.contains(&connection) {
                return Err(transport_error(ErrorCode::FramingError, "connection is closed"));
            }
            if let Some(error) = &log.send_error {
                return Err(error.clone());
            }
            log.sent.push((connection, output));
            Ok(())
        }

        fn close(&mut self, connection: ConnectionId, code: u16, reason: &str) -> Result<()> {
            let mut log = self.0.borrow_mut();
            log.close_calls += 1;
            if let Some(error) = &log.close_error {
                return Err(error.clone());
            }
            log.closes.push((connection, code, String::from(reason)));
            log.closed.push(connection);
            Ok(())
        }
    }

    fn binding() -> (ReactorBinding<RecordingTransport>, Rc<RefCell<TransportLog>>) {
        let log = Rc::new(RefCell::new(TransportLog::default()));
        (ReactorBinding::new(RecordingTransport(Rc::clone(&log))), log)
    }

    fn budget(retained_bytes: u32) -> ResourceBudget {
        ResourceBudget { cpu_units: 1, retained_bytes, timeout_ms: 1 }
    }

    fn input(bytes: &'static [u8]) -> ProtectedInput {
        receive::receive_bytes(Bytes::from_static(bytes), Opcode::Text, &WssPolicy::default())
            .unwrap()
    }

    fn diagnostic(code: ErrorCode, source: ErrorSource) -> Error {
        Error::new(code, source, 17, "preserve complete bridge diagnostic")
            .with_schema(29, 43)
    }

    fn assert_boundary_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Transport);
        assert_eq!(error.byte_offset, 0);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn admission_vectors_enforce_all_three_allowances_before_dispatch() {
        for cpu_units in [0, 1, u32::MAX] {
            for timeout_ms in [0, 1, u32::MAX] {
                for retained_bytes in [0, 3, 4, u32::MAX] {
                    let budget = ResourceBudget { cpu_units, retained_bytes, timeout_ms };
                    let mut dispatcher = RecordingDispatcher::default();
                    let result = submit_input(&mut dispatcher, ConnectionId(7), input(b"null"), budget);
                    if cpu_units == 0 || timeout_ms == 0 || retained_bytes < 4 {
                        assert_boundary_error(result, ErrorCode::BudgetExhausted);
                        assert_eq!(dispatcher.submit_calls, 0);
                        assert!(dispatcher.requests.is_empty());
                    } else {
                        result.unwrap();
                        assert_eq!(dispatcher.submit_calls, 1);
                        assert_eq!(dispatcher.requests.len(), 1);
                        let request = &dispatcher.requests[0];
                        assert_eq!(request.connection, ConnectionId(7));
                        assert_eq!(request.budget, budget);
                        assert_eq!(request.input.payload().as_slice(), b"null");
                    }
                    assert_eq!(dispatcher.poll_calls, 0);
                }
            }
        }
    }

    #[test]
    fn empty_input_can_use_zero_retained_bytes_but_needs_cpu_and_timeout() {
        let mut dispatcher = RecordingDispatcher::default();
        submit_input(&mut dispatcher, ConnectionId(0), input(b""), budget(0)).unwrap();
        assert_eq!(dispatcher.requests.len(), 1);
        assert!(dispatcher.requests[0].input.payload().is_empty());
        for invalid in [
            ResourceBudget { cpu_units: 0, ..budget(0) },
            ResourceBudget { timeout_ms: 0, ..budget(0) },
        ] {
            assert_boundary_error(
                submit_input(&mut dispatcher, ConnectionId(0), input(b""), invalid),
                ErrorCode::BudgetExhausted,
            );
        }
        assert_eq!(dispatcher.submit_calls, 1);
    }

    #[test]
    fn abi_length_and_retained_boundaries_never_truncate() {
        ensure_request_budget(0, budget(0)).unwrap();
        ensure_request_budget(3, budget(3)).unwrap();
        ensure_request_budget(u32::MAX as usize, budget(u32::MAX)).unwrap();
        assert_boundary_error(ensure_request_budget(4, budget(3)), ErrorCode::BudgetExhausted);
        assert_boundary_error(
            ensure_request_budget(u32::MAX as usize, budget(u32::MAX - 1)),
            ErrorCode::BudgetExhausted,
        );
        #[cfg(target_pointer_width = "64")]
        {
            assert_boundary_error(
                ensure_request_budget(u32::MAX as usize + 1, budget(u32::MAX)),
                ErrorCode::BudgetExhausted,
            );
            assert_boundary_error(
                ensure_request_budget(usize::MAX, budget(u32::MAX)),
                ErrorCode::BudgetExhausted,
            );
        }
    }

    #[test]
    fn submission_moves_sliced_bytes_without_copying_or_changing_metadata() {
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for opcode in [Opcode::Text, Opcode::Binary] {
            for connection in [ConnectionId(0), ConnectionId(1), ConnectionId(u64::MAX)] {
                let backing = Bytes::from(b"prefix{\"x\":1}suffix".to_vec());
                let bytes = backing.slice(6..13);
                let pointer = bytes.as_ptr();
                let input = receive::receive_bytes(bytes, opcode, &policy).unwrap();
                let mut dispatcher = RecordingDispatcher::default();
                let budget = ResourceBudget { cpu_units: 13, retained_bytes: 7, timeout_ms: 97 };
                submit_input(&mut dispatcher, connection, input, budget).unwrap();
                drop(backing);
                let request = dispatcher.requests.pop().unwrap();
                assert_eq!(request.connection, connection);
                assert_eq!(request.input.opcode(), opcode);
                assert_eq!(request.budget, budget);
                assert!(matches!(request.input.payload(), Payload::Bytes(_)));
                assert_eq!(request.input.payload().as_slice(), b"{\"x\":1}");
                assert_eq!(request.input.payload().as_slice().as_ptr(), pointer);
                assert_eq!(dispatcher.submit_calls, 1);
                assert_eq!(dispatcher.poll_calls, 0);
            }
        }
    }

    #[test]
    fn bridge_defers_json_and_routing_to_the_dispatcher() {
        let vectors: &[&[u8]] = &[
            b"null", b"{not JSON", b"null true",
            b"{\"seq\":7,\"code\":12,\"params\":{}}",
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}",
        ];
        for &bytes in vectors {
            let mut dispatcher = RecordingDispatcher::default();
            let input = receive::receive_bytes(
                Bytes::copy_from_slice(bytes), Opcode::Text, &WssPolicy::default(),
            ).unwrap();
            submit_input(&mut dispatcher, ConnectionId(19), input, budget(bytes.len() as u32))
                .unwrap();
            assert_eq!(dispatcher.requests[0].input.payload().as_slice(), bytes);
        }
    }

    #[test]
    fn dispatcher_admission_failures_propagate_without_polling_or_retry() {
        for (code, source) in [
            (ErrorCode::BudgetExhausted, ErrorSource::Transport),
            (ErrorCode::Cancelled, ErrorSource::Transport),
            (ErrorCode::AuthFailed, ErrorSource::Auth),
        ] {
            let error = diagnostic(code, source);
            let mut dispatcher = RecordingDispatcher {
                submit_error: Some(error.clone()), ..RecordingDispatcher::default()
            };
            assert_eq!(
                submit_input(&mut dispatcher, ConnectionId(3), input(b"null"), budget(4)),
                Err(error),
            );
            assert_eq!(dispatcher.submit_calls, 1);
            assert_eq!(dispatcher.poll_calls, 0);
            assert!(dispatcher.requests.is_empty());
        }
    }

    #[test]
    fn fragmented_unicode_input_crosses_the_bridge_only_after_completion() {
        let policy = WssPolicy::default();
        let mut fragments = FragmentState::new();
        let mut dispatcher = RecordingDispatcher::default();
        for (opcode, final_fragment, bytes) in [
            (Opcode::Text, false, b"\"\xf0\x9f".as_slice()),
            (Opcode::Ping, true, b"keepalive".as_slice()),
            (Opcode::Continuation, true, b"\x98\x80\"".as_slice()),
        ] {
            let event = receive::receive_frame(
                &mut fragments,
                IncomingFrame { opcode, final_fragment, payload: Payload::from_slice(bytes) },
                &policy,
            ).unwrap();
            match event {
                ReceiveEvent::Pending => assert_eq!(dispatcher.submit_calls, 0),
                ReceiveEvent::Ping(payload) => {
                    assert_eq!(payload.as_slice(), b"keepalive");
                    assert!(fragments.is_fragmented());
                    assert_eq!(dispatcher.submit_calls, 0);
                }
                ReceiveEvent::Message(input) => {
                    submit_input(&mut dispatcher, ConnectionId(23), input, budget(6)).unwrap();
                }
                other => panic!("unexpected receive event: {other:?}"),
            }
        }
        assert!(!fragments.is_fragmented());
        assert_eq!(dispatcher.submit_calls, 1);
        assert_eq!(dispatcher.requests[0].connection, ConnectionId(23));
        assert_eq!(dispatcher.requests[0].input.payload().as_slice(), "\"😀\"".as_bytes());
    }

    #[test]
    fn empty_poll_returns_false_and_never_calls_transport() {
        let (mut binding, log) = binding();
        let mut dispatcher = RecordingDispatcher::default();
        for count in 1..=3 {
            assert!(!forward_one(&mut binding, &mut dispatcher).unwrap());
            assert_eq!(dispatcher.poll_calls, count);
            assert_eq!(log.borrow().send_calls, 0);
            assert_eq!(log.borrow().close_calls, 0);
        }
        assert_eq!(dispatcher.submit_calls, 0);
    }

    #[test]
    fn an_empty_poll_does_not_drain_a_later_ready_completion() {
        let (mut binding, log) = binding();
        let error = diagnostic(ErrorCode::Cancelled, ErrorSource::Transport);
        let mut dispatcher = RecordingDispatcher::default();
        dispatcher.polls.push_back(Ok(None));
        dispatcher.polls.push_back(Ok(Some(BridgeCompletion {
            connection: ConnectionId(8), output: Err(error.clone()),
        })));
        assert!(!forward_one(&mut binding, &mut dispatcher).unwrap());
        assert_eq!(dispatcher.poll_calls, 1);
        assert_eq!(dispatcher.polls.len(), 1);
        assert_eq!(forward_one(&mut binding, &mut dispatcher), Err(error));
        assert_eq!(dispatcher.poll_calls, 2);
        assert!(dispatcher.polls.is_empty());
        assert_eq!(log.borrow().send_calls, 0);
    }

    #[test]
    fn poll_failure_preserves_the_next_completion() {
        let (mut binding, log) = binding();
        let poll_error = diagnostic(ErrorCode::BudgetExhausted, ErrorSource::Transport);
        let completion_error = diagnostic(ErrorCode::Cancelled, ErrorSource::Transport);
        let mut dispatcher = RecordingDispatcher::default();
        dispatcher.polls.push_back(Err(poll_error.clone()));
        dispatcher.polls.push_back(Ok(Some(BridgeCompletion {
            connection: ConnectionId(u64::MAX), output: Err(completion_error.clone()),
        })));
        assert_eq!(forward_one(&mut binding, &mut dispatcher), Err(poll_error));
        assert_eq!(dispatcher.poll_calls, 1);
        assert_eq!(dispatcher.polls.len(), 1);
        assert_eq!(forward_one(&mut binding, &mut dispatcher), Err(completion_error));
        assert_eq!(dispatcher.poll_calls, 2);
        assert!(dispatcher.polls.is_empty());
        assert!(!forward_one(&mut binding, &mut dispatcher).unwrap());
        assert_eq!(log.borrow().send_calls, 0);
        assert_eq!(log.borrow().close_calls, 0);
    }

    #[test]
    fn completion_failures_are_consumed_one_per_call_without_sending() {
        let (mut binding, log) = binding();
        let vectors = [
            diagnostic(ErrorCode::Cancelled, ErrorSource::Transport),
            diagnostic(ErrorCode::TypeMismatch, ErrorSource::SchemaVm),
            diagnostic(ErrorCode::InvalidUtf8, ErrorSource::Utf8),
        ];
        let mut dispatcher = RecordingDispatcher::default();
        for (index, error) in vectors.iter().enumerate() {
            dispatcher.polls.push_back(Ok(Some(BridgeCompletion {
                connection: ConnectionId(index as u64), output: Err(error.clone()),
            })));
        }
        for (index, error) in vectors.iter().enumerate() {
            assert_eq!(forward_one(&mut binding, &mut dispatcher), Err(error.clone()));
            assert_eq!(dispatcher.poll_calls, index + 1);
            assert_eq!(dispatcher.polls.len(), vectors.len() - index - 1);
            assert_eq!(log.borrow().send_calls, 0);
        }
        assert!(!forward_one(&mut binding, &mut dispatcher).unwrap());
    }

    // The frozen proof type has no safe constructor, and this crate forbids
    // unsafe code. These success vectors exercise the exact private delivery
    // helper used by forward_one with owned payloads, without fabricating a
    // ValidatedOutput or bypassing validation in the public API.
    #[test]
    fn delivery_preserves_connection_and_payload_ownership() {
        for connection in [ConnectionId(0), ConnectionId(42), ConnectionId(u64::MAX)] {
            for payload in [
                Payload::from_slice(b"{\"ok\":true}"),
                Payload::from_vec(b"{\"ok\":true}".to_vec()),
            ] {
                let pointer = payload.as_slice().as_ptr();
                let mut delivered = None;
                assert!(deliver_completion(Some((connection, Ok(payload))), |id, output| {
                    assert!(delivered.is_none());
                    delivered = Some((id, output));
                    Ok(())
                }).unwrap());
                let (id, output) = delivered.unwrap();
                assert_eq!(id, connection);
                assert_eq!(output.as_slice(), b"{\"ok\":true}");
                assert_eq!(output.as_slice().as_ptr(), pointer);
            }
        }
    }

    #[test]
    fn delivery_skips_sender_for_absence_and_output_errors() {
        assert!(!deliver_completion::<Payload>(None, |_, _| {
            panic!("an empty completion must never send")
        }).unwrap());
        let error = diagnostic(ErrorCode::MissingRequiredField, ErrorSource::SchemaVm);
        assert_eq!(deliver_completion::<Payload>(
            Some((ConnectionId(9), Err(error.clone()))),
            |_, _| panic!("a failed completion must never send"),
        ), Err(error));
    }

    #[test]
    fn transport_failures_and_late_closed_completions_are_never_retried() {
        for code in [ErrorCode::BudgetExhausted, ErrorCode::FramingError, ErrorCode::Cancelled] {
            let error = diagnostic(code, ErrorSource::Transport);
            let mut queue = VecDeque::new();
            queue.push_back((ConnectionId(5), Ok(Payload::from_slice(b"null"))));
            queue.push_back((ConnectionId(6), Ok(Payload::from_slice(b"true"))));
            let mut calls = 0;
            let mut accepted = Vec::new();
            assert_eq!(deliver_completion(queue.pop_front(), |connection, _| {
                calls += 1;
                assert_eq!(connection, ConnectionId(5));
                // A full queue or a closed connection accepts no frame.
                Err(error.clone())
            }), Err(error));
            assert_eq!(calls, 1);
            assert_eq!(queue.len(), 1);
            assert!(accepted.is_empty());
            assert!(deliver_completion(queue.pop_front(), |connection, output| {
                calls += 1;
                accepted.push((connection, output));
                Ok(())
            }).unwrap());
            assert_eq!(calls, 2);
            assert_eq!(accepted.len(), 1);
            assert_eq!(accepted[0].0, ConnectionId(6));
            assert_eq!(accepted[0].1.as_slice(), b"true");
            assert!(queue.is_empty());
        }
    }

    struct DropProbe(Rc<Cell<usize>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn delivery_moves_nonclone_values_and_drops_failed_sends_once() {
        let drops = Rc::new(Cell::new(0));
        let mut retained = None;
        assert!(deliver_completion(
            Some((ConnectionId(1), Ok(DropProbe(Rc::clone(&drops))))),
            |_, output| {
                retained = Some(output);
                Ok(())
            },
        ).unwrap());
        assert_eq!(drops.get(), 0);
        drop(retained.take());
        assert_eq!(drops.get(), 1);
        let error = diagnostic(ErrorCode::BudgetExhausted, ErrorSource::Transport);
        assert_eq!(deliver_completion(
            Some((ConnectionId(1), Ok(DropProbe(Rc::clone(&drops))))),
            |_, _output| Err(error.clone()),
        ), Err(error));
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn close_codes_match_receive_policy_for_every_u16_value() {
        let (mut binding, log) = binding();
        let mut legal = 0;
        for code in u16::MIN..=u16::MAX {
            let expected = matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999);
            let received = policy::validate_close_payload(&code.to_be_bytes()).map(|(code, _)| code);
            let result = close_connection(&mut binding, ConnectionId(11), code, "");
            if expected {
                result.unwrap();
                assert_eq!(received.unwrap(), Some(code));
                legal += 1;
                let log = log.borrow();
                assert_eq!(log.close_calls, legal);
                assert_eq!(log.closes.last(), Some(&(ConnectionId(11), code, String::new())));
            } else {
                assert_boundary_error(result, ErrorCode::FramingError);
                assert_boundary_error(received, ErrorCode::FramingError);
                assert_eq!(log.borrow().close_calls, legal);
            }
        }
        assert_eq!(legal, 2012);
        assert_eq!(log.borrow().send_calls, 0);
    }

    #[test]
    fn close_reason_boundaries_count_utf8_bytes_and_preserve_contents() {
        let (mut binding, log) = binding();
        let vectors = [
            String::new(), String::from("bye"), String::from("\0\n{not JSON"),
            "a".repeat(122), "a".repeat(123), "水".repeat(41),
            alloc::format!("{}abc", "😀".repeat(30)),
        ];
        for (index, reason) in vectors.iter().enumerate() {
            close_connection(&mut binding, ConnectionId(u64::MAX), 3000, reason).unwrap();
            let log = log.borrow();
            assert_eq!(log.close_calls, index + 1);
            assert_eq!(log.closes[index], (ConnectionId(u64::MAX), 3000, reason.clone()));
        }
        let accepted = log.borrow().close_calls;
        for reason in ["a".repeat(124), "水".repeat(42), "😀".repeat(31), "a".repeat(4096)] {
            assert_boundary_error(
                close_connection(&mut binding, ConnectionId(0), 1000, &reason),
                ErrorCode::PayloadTooLarge,
            );
            assert_eq!(log.borrow().close_calls, accepted);
        }
        assert_eq!(log.borrow().send_calls, 0);
    }

    #[test]
    fn invalid_close_arguments_do_not_reach_even_a_failing_transport() {
        let (mut binding, log) = binding();
        let error = diagnostic(ErrorCode::Cancelled, ErrorSource::Transport);
        log.borrow_mut().close_error = Some(error.clone());
        assert_boundary_error(
            close_connection(&mut binding, ConnectionId(1), 1005, "bye"),
            ErrorCode::FramingError,
        );
        assert_boundary_error(
            close_connection(&mut binding, ConnectionId(1), 1000, &"a".repeat(124)),
            ErrorCode::PayloadTooLarge,
        );
        assert_eq!(log.borrow().close_calls, 0);
        assert_eq!(close_connection(&mut binding, ConnectionId(1), 1000, "bye"), Err(error));
        assert_eq!(log.borrow().close_calls, 1);
        assert!(log.borrow().closes.is_empty());
        assert!(log.borrow().closed.is_empty());
    }

    #[test]
    fn close_preserves_connection_identity_and_does_not_poll_dispatcher() {
        let (mut binding, log) = binding();
        let mut dispatcher = RecordingDispatcher::default();
        submit_input(&mut dispatcher, ConnectionId(31), input(b"null"), budget(4)).unwrap();
        close_connection(&mut binding, ConnectionId(31), 1001, "going away").unwrap();
        assert_eq!(dispatcher.submit_calls, 1);
        assert_eq!(dispatcher.poll_calls, 0);
        assert_eq!(dispatcher.requests.len(), 1);
        assert_eq!(log.borrow().closed.as_slice(), &[ConnectionId(31)]);
        assert_eq!(log.borrow().closes[0], (ConnectionId(31), 1001, String::from("going away")));
        assert_eq!(log.borrow().send_calls, 0);
    }

    #[test]
    fn owned_seam_values_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BridgeRequest>();
        assert_send_sync::<BridgeCompletion>();
        assert_send_sync::<ProtectedInput>();
        assert_send_sync::<ValidatedOutput>();
    }
}
