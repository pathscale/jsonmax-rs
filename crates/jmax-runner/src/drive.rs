//! Sequential scenario execution, strict replies, and structural assertions.
//!
//! Transports own scheduling and enforce the positive relative I/O deadline.
//! Execution state borrows scenario names and owns saved replies. Cleanup is a
//! separate pass so every active connection is attempted even after an error.

use super::*;
use jmax_dom::{navigate, scalar, Document, Node, NodeKind};

/// Validate exactly one complete JSON value under the canonical strict limits.
pub fn validate_reply(input: &[u8]) -> Result<()> {
    let limits = Limits::strict_default();
    if input.len() > limits.max_payload_bytes as usize {
        return Err(Error::new(ErrorCode::PayloadTooLarge, ErrorSource::Decode,
            limits.max_payload_bytes, "reply exceeds strict payload limit").into());
    }
    let mut owned = Vec::new();
    owned.try_reserve_exact(input.len()).map_err(|_| allocation_error())?;
    owned.extend_from_slice(input);
    parse_reply(Payload::from_vec(owned))?;
    Ok(())
}

/// Assert against a fully validated reply, distinguishing missing from null.
///
/// Native paths select at most one value. Object order and escape spelling do
/// not affect equality, but array order and number lexemes do. Diagnostics never
/// include the selected value, expected value, path, or reply body.
pub fn assert_reply(reply: &Reply, path: &str, expected: &Expectation) -> Result<()> {
    let document = parse_reply(reply.body.clone())?;
    let compiled = jmax_path::compile::compile_strings(&[path], jmax_path::PathLimits::default())
        .map_err(|_| invalid(None, "assert.path must be a valid native selector path within path limits"))?;
    if let Expectation::ErrorCode(code) = expected {
        if !path.is_empty() {
            return Err(invalid(None, "error_code requires an empty assert.path"));
        }
        return if reply.error_code == Some(*code) { Ok(()) }
            else { Err(assertion("reply error code does not match")) };
    }

    // The compiler owns one path in slot zero. Traverse that exact native path
    // against the already validated DOM, avoiding a second payload scan.
    let native = compiled.paths().first().ok_or_else(|| internal_error(
        "compiled assertion path is absent"))?;
    let selected = select(&document, native.segments());
    match expected {
        Expectation::Exists if selected.is_some() => Ok(()),
        Expectation::Missing if selected.is_none() => Ok(()),
        Expectation::Exists => Err(assertion("assertion path is missing")),
        Expectation::Missing => Err(assertion("assertion path exists")),
        Expectation::JsonEquals(payload) => {
            // Validate the expectation even when selection is missing.
            let expected_document = parse_reply(payload.clone())?;
            match selected {
                Some(actual) if structurally_equal(actual, navigate::root(&expected_document))? => Ok(()),
                _ => Err(assertion("selected JSON value does not match")),
            }
        }
        Expectation::ErrorCode(_) => Err(internal_error("error code assertion was not dispatched")),
    }
}

fn parse_reply(payload: Payload) -> Result<Document> {
    Ok(jmax_dom::build::parse(payload, Limits::strict_default())?)
}

fn select<'a>(document: &'a Document, segments: &[jmax_path::Segment]) -> Option<Node<'a>> {
    let mut selected = navigate::root(document);
    for segment in segments {
        selected = match segment {
            jmax_path::Segment::Key(key) => navigate::object(selected).ok()?.get(key)?,
            jmax_path::Segment::Index(index) => {
                let index = usize::try_from(*index).ok()?;
                navigate::array(selected).ok()?.get(index)?
            }
        };
    }
    Some(selected)
}

// Explicit work storage avoids recursive Rust calls on attacker-controlled JSON
// depth. Sort each pair of object member lists once instead of quadratic lookup.
fn structurally_equal<'a, 'b>(left: Node<'a>, right: Node<'b>) -> Result<bool> {
    let mut pending = Vec::new();
    pending.try_reserve(1).map_err(|_| allocation_error())?;
    pending.push((left, right));
    while let Some((left, right)) = pending.pop() {
        let kind = navigate::kind(left);
        if kind != navigate::kind(right) { return Ok(false); }
        match kind {
            NodeKind::Array => {
                let left = navigate::array(left)?;
                let right = navigate::array(right)?;
                if left.len() != right.len() { return Ok(false); }
                pending.try_reserve(left.len()).map_err(|_| allocation_error())?;
                pending.extend(left.iter().zip(right.iter()));
            }
            NodeKind::Object => {
                let left = navigate::object(left)?;
                let right = navigate::object(right)?;
                if left.len() != right.len() { return Ok(false); }
                let mut left_members = Vec::new();
                let mut right_members = Vec::new();
                left_members.try_reserve_exact(left.len()).map_err(|_| allocation_error())?;
                right_members.try_reserve_exact(right.len()).map_err(|_| allocation_error())?;
                left_members.extend(left.iter());
                right_members.extend(right.iter());
                left_members.sort_unstable_by(|a, b| a.key.cmp(b.key));
                right_members.sort_unstable_by(|a, b| a.key.cmp(b.key));
                pending.try_reserve(left.len()).map_err(|_| allocation_error())?;
                for (a, b) in left_members.into_iter().zip(right_members) {
                    if a.key != b.key { return Ok(false); }
                    pending.push((a.value, b.value));
                }
            }
            _ => {
                if scalar::value(left)? != scalar::value(right)? { return Ok(false); }
            }
        }
    }
    Ok(true)
}

type Slots<'a, T> = Vec<(&'a str, Option<T>)>;
type Counters<'a> = Vec<(&'a str, u32, u64)>;

/// Validate the entire model before I/O, then execute and clean up in order.
///
/// Connection and response tables are reserved before connecting. A failed
/// explicit close remains active for the final lexical cleanup pass. Transport
/// errors are propagated intact under Transport's sanitized-error contract.
pub fn run_scenario(scenario: &Scenario, transport: &mut dyn Transport) -> Result<RunReport> {
    scenario::validate_scenario(scenario)?;
    let mut connections = slots(scenario.steps.iter().filter_map(|step| match step {
        Step::Connect { name, .. } => Some(name.as_str()),
        _ => None,
    }))?;
    let mut responses = slots(scenario.steps.iter().filter_map(|step| match step {
        Step::Call { save_as, .. } | Step::Receive { save_as, .. } => Some(save_as.as_str()),
        _ => None,
    }))?;
    let mut counters = planned_counters(scenario, &mut connections)?;
    let mut report = RunReport { scenario: owned_string(&scenario.name)?, ..RunReport::default() };
    let execution = execute(scenario, transport, &mut connections, &mut responses,
        &mut counters, &mut report);
    let cleanup = cleanup(transport, &mut connections);
    // Saved payload owners and their transport resources never escape the run.
    drop(responses);
    execution?;
    cleanup?;
    for (role, endpoint, count) in counters {
        if count != 0 {
            report.calls.insert(CoverageKey { role: owned_string(role)?, endpoint }, count);
        }
    }
    Ok(report)
}

fn slots<'a, T>(names: impl Iterator<Item = &'a str>) -> Result<Slots<'a, T>> {
    let mut table = Vec::new();
    for name in names {
        table.try_reserve(1).map_err(|_| allocation_error())?;
        table.push((name, None));
    }
    table.sort_unstable_by(|a, b| a.0.cmp(b.0));
    table.dedup_by(|a, b| a.0 == b.0);
    Ok(table)
}

fn slot_mut<'s, T>(slots: &'s mut [(&str, Option<T>)], name: &str) -> Result<&'s mut Option<T>> {
    let index = slots.binary_search_by(|entry| entry.0.cmp(name))
        .map_err(|_| internal_error("validated execution name is absent"))?;
    Ok(&mut slots[index].1)
}

fn active_role<'a>(connections: &[(&str, Option<&'a str>)], name: &str) -> Result<&'a str> {
    let index = connections.binary_search_by(|entry| entry.0.cmp(name))
        .map_err(|_| internal_error("validated connection is absent"))?;
    connections[index].1.ok_or_else(|| internal_error("validated connection is inactive"))
}

// Reserve and deduplicate role/endpoint counters before I/O. Reused connection
// names resolve to the role from their most recent connect, never the first one.
fn planned_counters<'a>(scenario: &'a Scenario, connections: &mut Slots<'a, &'a str>) -> Result<Counters<'a>> {
    let mut counters = Vec::new();
    for step in &scenario.steps {
        match step {
            Step::Connect { name, role, .. } => *slot_mut(connections, name)? = Some(role.as_str()),
            Step::Close { connection } => *slot_mut(connections, connection)? = None,
            Step::Call { connection, endpoint, .. } => {
                let role = active_role(connections, connection)?;
                counters.try_reserve(1).map_err(|_| allocation_error())?;
                counters.push((role, *endpoint, 0));
            }
            _ => {}
        }
    }
    for (_, role) in connections { *role = None; }
    counters.sort_unstable_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
    counters.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    Ok(counters)
}

fn execute<'a>(
    scenario: &'a Scenario,
    transport: &mut dyn Transport,
    connections: &mut Slots<'a, &'a str>,
    responses: &mut Slots<'a, Reply>,
    counters: &mut Counters<'a>,
    report: &mut RunReport,
) -> Result<()> {
    let mut sequence = 0u64;
    for (index, step) in scenario.steps.iter().enumerate() {
        match step {
            Step::Connect { name, uri, role } => {
                // Locate storage before the successful connect creates a resource.
                let state = slot_mut(connections, name)?;
                transport.connect(name, uri, role, scenario.timeout_ms)?;
                *state = Some(role.as_str());
            }
            Step::Call { connection, endpoint, params, save_as } => {
                let role = active_role(connections, connection)?;
                sequence = next_u64(sequence, index, "call sequence overflow")?;
                let request = CallRequest { endpoint: *endpoint, sequence, params: params.clone() };
                let reply = transport.call(connection, &request, scenario.timeout_ms)?;
                if reply.sequence != Some(sequence) {
                    return Err(RunnerError::Transport {
                        connection: owned_string(connection)?,
                        detail: owned_string("call reply sequence does not match request")?,
                    });
                }
                parse_reply(reply.body.clone())?;
                let counter = counters.binary_search_by(|entry|
                    entry.0.cmp(role).then_with(|| entry.1.cmp(endpoint)))
                    .map_err(|_| internal_error("validated call counter is absent"))?;
                counters[counter].2 = next_u64(counters[counter].2, index, "call counter overflow")?;
                *slot_mut(responses, save_as)? = Some(reply);
            }
            Step::Receive { connection, save_as } => {
                active_role(connections, connection)?;
                let reply = transport.receive(connection, scenario.timeout_ms)?;
                parse_reply(reply.body.clone())?;
                *slot_mut(responses, save_as)? = Some(reply);
            }
            Step::Assert { response, path, expected } => {
                let reply = slot_mut(responses, response)?.as_ref()
                    .ok_or_else(|| internal_error("validated response has not been saved"))?;
                assert_reply(reply, path, expected).map_err(|error| match error {
                    RunnerError::Assertion { detail, .. } => RunnerError::Assertion { step: index, detail },
                    RunnerError::InvalidScenario { detail, .. } => RunnerError::InvalidScenario {
                        step: Some(index), detail,
                    },
                    error => error,
                })?;
                report.assertions_passed = next_usize(report.assertions_passed, index,
                    "assertion counter overflow")?;
            }
            Step::Close { connection } => {
                let state = slot_mut(connections, connection)?;
                transport.close(connection)?;
                *state = None;
            }
        }
        report.steps_completed = next_usize(report.steps_completed, index, "step counter overflow")?;
    }
    Ok(())
}

fn cleanup(transport: &mut dyn Transport, connections: &mut [(&str, Option<&str>)]) -> Result<()> {
    let mut first_error = None;
    for (name, role) in connections {
        if role.take().is_some() {
            if let Err(error) = transport.close(name) {
                if first_error.is_none() { first_error = Some(error); }
            }
        }
    }
    match first_error { Some(error) => Err(error), None => Ok(()) }
}

fn next_u64(value: u64, step: usize, detail: &'static str) -> Result<u64> {
    value.checked_add(1).ok_or_else(|| invalid(Some(step), detail))
}

fn next_usize(value: usize, step: usize, detail: &'static str) -> Result<usize> {
    value.checked_add(1).ok_or_else(|| invalid(Some(step), detail))
}

fn owned_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| allocation_error())?;
    owned.push_str(value);
    Ok(owned)
}

fn allocation_error() -> RunnerError {
    Error::new(ErrorCode::BudgetExhausted, ErrorSource::Decode, 0,
        "runner allocation failed").into()
}

fn internal_error(detail: &'static str) -> RunnerError {
    Error::new(ErrorCode::InternalError, ErrorSource::Decode, 0, detail).into()
}

fn invalid(step: Option<usize>, detail: &'static str) -> RunnerError {
    match owned_string(detail) {
        Ok(detail) => RunnerError::InvalidScenario { step, detail },
        Err(error) => error,
    }
}

fn assertion(detail: &'static str) -> RunnerError {
    match owned_string(detail) {
        Ok(detail) => RunnerError::Assertion { step: 0, detail },
        Err(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::VecDeque, format, string::ToString, sync::{Arc, Weak}, vec};

    fn reply(body: &[u8], sequence: Option<u64>, error_code: Option<u32>) -> Reply {
        Reply { body: Payload::from_slice(body), sequence, error_code }
    }

    fn equals(body: &[u8]) -> Expectation { Expectation::JsonEquals(Payload::from_slice(body)) }

    fn model(steps: Vec<Step>) -> Scenario {
        Scenario { name: "deterministic".into(), timeout_ms: 17, steps }
    }

    fn connect(name: &str, role: &str) -> Step {
        Step::Connect { name: name.into(), uri: "custom://transport".into(), role: role.into() }
    }

    fn call(name: &str, endpoint: u32, save_as: &str) -> Step {
        Step::Call { connection: name.into(), endpoint, params: Payload::from_slice(b"{\"x\":1}"),
            save_as: save_as.into() }
    }

    fn receive(name: &str, save_as: &str) -> Step {
        Step::Receive { connection: name.into(), save_as: save_as.into() }
    }

    fn assert_step(response: &str, path: &str, expected: Expectation) -> Step {
        Step::Assert { response: response.into(), path: path.into(), expected }
    }

    fn close(name: &str) -> Step { Step::Close { connection: name.into() } }

    fn transport_error(operation: &str) -> RunnerError {
        RunnerError::Transport { connection: "safe-context".into(), detail: operation.into() }
    }

    fn assert_json<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) -> Error {
        match result.unwrap_err() {
            RunnerError::Json(error) => { assert_eq!(error.code, code); error }
            other => panic!("expected JSON error, got {other:?}"),
        }
    }

    fn assert_mismatch(result: Result<()>, expected_step: usize) {
        match result.unwrap_err() {
            RunnerError::Assertion { step, detail } => {
                assert_eq!(step, expected_step);
                assert!(!detail.is_empty());
            }
            other => panic!("expected assertion error, got {other:?}"),
        }
    }

    #[derive(Default)]
    struct Scripted {
        events: Vec<String>,
        deadlines: Vec<u32>,
        requests: Vec<(String, CallRequest)>,
        replies: VecDeque<Result<Reply>>,
        connect_failure: Option<String>,
        close_failures: BTreeMap<String, usize>,
        watched: Vec<Weak<[u8]>>,
        live_at_close: Vec<Vec<bool>>,
    }

    impl Scripted {
        fn with_replies(replies: Vec<Reply>) -> Self {
            Self { replies: replies.into_iter().map(Ok).collect(), ..Self::default() }
        }

        fn next_reply(&mut self) -> Result<Reply> {
            self.replies.pop_front().unwrap_or_else(|| Err(transport_error("script exhausted")))
        }
    }

    impl Transport for Scripted {
        fn connect(&mut self, name: &str, _uri: &str, _role: &str, timeout_ms: u32) -> Result<()> {
            self.events.push(format!("connect:{name}"));
            self.deadlines.push(timeout_ms);
            if self.connect_failure.as_deref() == Some(name) {
                Err(transport_error("connect failed"))
            } else { Ok(()) }
        }

        fn call(&mut self, name: &str, request: &CallRequest, timeout_ms: u32) -> Result<Reply> {
            self.events.push(format!("call:{name}"));
            self.deadlines.push(timeout_ms);
            self.requests.push((name.into(), request.clone()));
            self.next_reply()
        }

        fn receive(&mut self, name: &str, timeout_ms: u32) -> Result<Reply> {
            self.events.push(format!("receive:{name}"));
            self.deadlines.push(timeout_ms);
            self.next_reply()
        }

        fn close(&mut self, name: &str) -> Result<()> {
            self.events.push(format!("close:{name}"));
            self.live_at_close.push(self.watched.iter().map(|weak| weak.upgrade().is_some()).collect());
            if let Some(remaining) = self.close_failures.get_mut(name) {
                if *remaining != 0 {
                    *remaining -= 1;
                    return Err(transport_error(&format!("close failed:{name}")));
                }
            }
            Ok(())
        }
    }

    #[test]
    fn strict_reply_accepts_all_json_kinds_and_exact_numeric_lexemes() {
        for body in ["null", "true", "false", "0", "-0", "1.0", "1e+99", "1e99999",
            "1844674407370955161600000", r#""text\n\uD83D\uDE00""#,
            "[]", "{}", r#" {"unknown":[null,false,{"a":1}]} "#] {
            assert!(validate_reply(body.as_bytes()).is_ok(), "{body}");
        }
        assert!(validate_reply(b" \t\r\n {\"unknown\": [0]} \n").is_ok());
    }

    #[test]
    fn strict_reply_rejects_invalid_json_and_preserves_abi_offsets() {
        let bodies: &[&[u8]] = &[
            b"", b" ", b"null true", b"[1,]", b"{\"a\":0,}", b"/*x*/null", b"01",
            b"+1", b"1.", b"NaN", b"1e", b"\"\\q\"", b"\"\\uD800\"",
            b"\"\\uDC00\"", b"\"\xff\"", b"\"line\nbreak\"", b"[", b"{",
        ];
        for body in bodies {
            let expected = jmax_dom::build::parse(Payload::from_slice(body), Limits::strict_default()).unwrap_err();
            match validate_reply(body).unwrap_err() {
                RunnerError::Json(actual) => assert_eq!(actual, expected),
                other => panic!("expected original JSON error, got {other:?}"),
            }
        }
        assert_json(validate_reply(b"null x"), ErrorCode::TrailingContent);
        let duplicate = assert_json(validate_reply(br#"{"a":0,"\u0061":1}"#), ErrorCode::DuplicateObjectKey);
        assert_eq!(duplicate.byte_offset, 7);
        assert_json(validate_reply(br#"{"nested":{"x":1,"x":2}}"#), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn strict_payload_depth_string_and_digit_boundaries() {
        let limits = Limits::strict_default();
        let mut bytes = b"null".to_vec();
        bytes.resize(limits.max_payload_bytes as usize, b' ');
        assert!(validate_reply(&bytes).is_ok());
        bytes.push(b' ');
        let error = assert_json(validate_reply(&bytes), ErrorCode::PayloadTooLarge);
        assert_eq!(error.byte_offset, limits.max_payload_bytes);
        let depth = limits.max_depth as usize;
        let allowed = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        assert!(validate_reply(allowed.as_bytes()).is_ok());
        let excessive = format!("[{allowed}]");
        assert_json(validate_reply(excessive.as_bytes()), ErrorCode::DepthLimitExceeded);
        let text = format!("\"{}\"", "a".repeat(limits.max_string_bytes as usize));
        assert!(validate_reply(text.as_bytes()).is_ok());
        let excessive = format!("\"{}a\"", "a".repeat(limits.max_string_bytes as usize));
        assert!(matches!(validate_reply(excessive.as_bytes()), Err(RunnerError::Json(_))));
        let number = "1".repeat(limits.max_number_digits as usize);
        assert!(validate_reply(number.as_bytes()).is_ok());
        assert!(matches!(validate_reply(format!("{number}1").as_bytes()), Err(RunnerError::Json(_))));
    }

    #[test]
    fn native_paths_distinguish_missing_null_and_wrong_container_types() {
        let body = reply(br#"{"a":[null,{"b":true}],"a.b":7,"":false,"\u03bb":"ok"}"#, None, None);
        for path in ["", "$", "a[0]", "$.a[1].b", r"a\.b", r#"["a.b"]"#, r#"[""]"#, "λ"] {
            assert!(assert_reply(&body, path, &Expectation::Exists).is_ok(), "{path}");
            assert_mismatch(assert_reply(&body, path, &Expectation::Missing), 0);
        }
        for path in ["absent", "a[2]", "a[4294967295]", "a[0].x", "a.b", "a[1][0]", "λ.x"] {
            assert!(assert_reply(&body, path, &Expectation::Missing).is_ok(), "{path}");
            assert_mismatch(assert_reply(&body, path, &Expectation::Exists), 0);
            assert_mismatch(assert_reply(&body, path, &equals(b"null")), 0);
        }
        assert!(assert_reply(&body, "a[0]", &equals(b"null")).is_ok());
        for literal in [b"null".as_slice(), b"false", b"[]", b"{}", b"\"\""] {
            let body = reply(literal, None, None);
            assert!(assert_reply(&body, "", &Expectation::Exists).is_ok());
            assert_mismatch(assert_reply(&body, "", &Expectation::Missing), 0);
        }
    }

    #[test]
    fn malformed_paths_are_model_errors_without_echoing_the_path() {
        let body = reply(b"null", None, None);
        for path in [".", "a..b", "a[01]", "a[-1]", "a[4294967296]", "*", "a[*]", " a", "secret..credential"] {
            match assert_reply(&body, path, &Expectation::Exists).unwrap_err() {
                RunnerError::InvalidScenario { step, detail } => {
                    assert_eq!(step, None);
                    assert!(!detail.contains("credential"));
                }
                other => panic!("expected path error, got {other:?}"),
            }
        }
        let excessive = "a".repeat(jmax_path::PathLimits::default().max_path_bytes as usize + 1);
        assert!(matches!(assert_reply(&body, &excessive, &Expectation::Exists),
            Err(RunnerError::InvalidScenario { .. })));
    }

    #[test]
    fn structural_equality_ignores_object_order_and_decodes_strings_and_keys() {
        let actual = reply(br#"{"a":[{"x":"\uD83D\uDE00","y":true},null],"\u0062":{"q":1e+2}}"#, None, None);
        let expected = r#"{"b":{"q":1e+2},"a":[{"y":true,"x":"😀"},null]}"#;
        assert!(assert_reply(&actual, "", &equals(expected.as_bytes())).is_ok());
        assert!(assert_reply(&reply(br#""a\/b\n\u0063""#, None, None), "",
            &equals(br#""a/b\nc""#)).is_ok());
        for (a, b) in [("{}", "[]"), ("null", "false"), ("true", "false"), ("1", "\"1\""),
            ("1", "1.0"), ("1e0", "1E0"), ("1e+0", "1e0"), ("-0", "0"),
            ("[1,2]", "[2,1]"), ("[1]", "[1,2]"), (r#"{"a":1}"#, r#"{"b":1}"#),
            (r#"{"a":1}"#, r#"{"a":1,"b":2}"#), (r#""é""#, r#""é""#)] {
            assert_mismatch(assert_reply(&reply(a.as_bytes(), None, None), "", &equals(b.as_bytes())), 0);
        }
        let deep = format!("{}0{}", "[".repeat(100), "]".repeat(100));
        assert!(assert_reply(&reply(deep.as_bytes(), None, None), "", &equals(deep.as_bytes())).is_ok());
    }

    #[test]
    fn assertions_validate_unselected_content_and_expectations_before_mismatch() {
        let malformed = reply(br#"{"selected":1,"other":[0,]}"#, None, Some(9));
        for expected in [Expectation::Exists, Expectation::Missing, equals(b"1"), Expectation::ErrorCode(9)] {
            assert!(matches!(assert_reply(&malformed, "", &expected), Err(RunnerError::Json(_))));
        }
        let valid = reply(b"{}", None, None);
        for expected in [b"[0,]".as_slice(), br#"{"a":0,"\u0061":1}"#, b"null true"] {
            assert!(matches!(assert_reply(&valid, "missing", &equals(expected)), Err(RunnerError::Json(_))));
        }
        assert_json(assert_reply(&reply(br#"{"x":0,"x":1}"#, None, None), "x", &equals(b"0")),
            ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn error_code_uses_only_metadata_and_requires_a_literally_empty_path() {
        for code in [0, 1, u32::MAX] {
            let body = reply(br#"{"error_code":7}"#, None, Some(code));
            assert!(assert_reply(&body, "", &Expectation::ErrorCode(code)).is_ok());
            assert_mismatch(assert_reply(&body, "", &Expectation::ErrorCode(code.wrapping_add(1))), 0);
            for path in ["$", "error_code"] {
                assert!(matches!(assert_reply(&body, path, &Expectation::ErrorCode(code)),
                    Err(RunnerError::InvalidScenario { step: None, .. })));
            }
        }
        assert_mismatch(assert_reply(&reply(br#"{"error_code":0}"#, None, None), "", &Expectation::ErrorCode(0)), 0);
    }

    #[test]
    fn empty_run_and_invalid_models_perform_no_io() {
        let mut transport = Scripted::default();
        let report = run_scenario(&model(vec![]), &mut transport).unwrap();
        assert_eq!(report.scenario, "deterministic");
        assert_eq!((report.steps_completed, report.assertions_passed), (0, 0));
        assert!(report.calls.is_empty());
        let mut zero_timeout = model(vec![connect("c", "")]);
        zero_timeout.timeout_ms = 0;
        let mut empty_name = model(vec![]);
        empty_name.name.clear();
        for invalid in [zero_timeout, empty_name,
            model(vec![connect("c", ""), call("absent", 1, "r")]),
            model(vec![connect("c", ""), receive("c", "r"), assert_step("r", "*", Expectation::Exists)]),
            model(vec![connect("c", ""), receive("c", "r"), assert_step("r", "", equals(b"[0,]"))])] {
            assert!(run_scenario(&invalid, &mut transport).is_err());
        }
        assert!(transport.events.is_empty());
    }

    #[test]
    fn calls_use_global_sequences_deadlines_params_and_application_error_coverage() {
        let scenario = model(vec![connect("z", "public"), connect("a", "public"),
            call("z", 0, "one"), receive("a", "push"), call("a", 0, "two"),
            assert_step("one", "", Expectation::ErrorCode(u32::MAX)),
            assert_step("push", "value", equals(b"true")),
            close("z"), connect("z", "private"), call("z", u32::MAX, "three")]);
        let mut transport = Scripted::with_replies(vec![reply(b"null", Some(1), Some(u32::MAX)),
            reply(br#"{"value":true}"#, Some(777), None), reply(b"{}", Some(2), None), reply(b"[]", Some(3), None)]);
        let report = run_scenario(&scenario, &mut transport).unwrap();
        assert_eq!((report.steps_completed, report.assertions_passed), (10, 2));
        assert_eq!(report.calls.len(), 2);
        assert_eq!(report.calls.get(&CoverageKey { role: "public".into(), endpoint: 0 }), Some(&2));
        assert_eq!(report.calls.get(&CoverageKey { role: "private".into(), endpoint: u32::MAX }), Some(&1));
        assert!(transport.deadlines.iter().all(|deadline| *deadline == 17));
        assert_eq!(transport.requests.iter().map(|entry| entry.1.sequence).collect::<Vec<_>>(), vec![1, 2, 3]);
        for (_, request) in &transport.requests { assert_eq!(request.params.as_slice(), b"{\"x\":1}"); }
        assert_eq!(&transport.events[transport.events.len() - 2..], ["close:a", "close:z"]);
    }

    #[test]
    fn receives_accept_absent_or_arbitrary_sequences_and_do_not_count_as_calls() {
        let scenario = model(vec![connect("c", ""), receive("c", "r"), receive("c", "s"),
            close("c"), assert_step("r", "", Expectation::Exists), assert_step("s", "", equals(b"null"))]);
        let mut transport = Scripted::with_replies(vec![reply(b"{}", None, None), reply(b"null", Some(u64::MAX), None)]);
        let report = run_scenario(&scenario, &mut transport).unwrap();
        assert_eq!((report.steps_completed, report.assertions_passed), (6, 2));
        assert!(report.calls.is_empty());
        assert_eq!(transport.events.iter().filter(|event| *event == "close:c").count(), 1);
    }

    #[test]
    fn connection_reuse_updates_roles_and_case_sensitive_names_stay_distinct() {
        let scenario = model(vec![connect("c", "public"), connect("C", "Public"),
            call("c", 7, "r"), call("C", 7, "R"), close("c"), connect("c", ""),
            call("c", 7, "last"), close("C"), close("c"),
            assert_step("r", "", equals(b"1")), assert_step("R", "", equals(b"2")),
            assert_step("last", "", equals(b"3"))]);
        let mut transport = Scripted::with_replies(vec![reply(b"1", Some(1), None),
            reply(b"2", Some(2), None), reply(b"3", Some(3), None)]);
        let report = run_scenario(&scenario, &mut transport).unwrap();
        assert_eq!(report.calls.len(), 3);
        for role in ["public", "Public", ""] {
            assert_eq!(report.calls.get(&CoverageKey { role: role.into(), endpoint: 7 }), Some(&1));
        }
        assert_eq!(transport.events.iter().filter(|event| *event == "close:c").count(), 2);
        assert_eq!(transport.events.iter().filter(|event| *event == "close:C").count(), 1);
        assert_eq!((report.steps_completed, report.assertions_passed), (12, 3));
    }

    #[test]
    fn successful_cleanup_is_lexical_including_unicode_and_ignores_closed_names() {
        let scenario = model(vec![connect("λ", ""), connect("z", ""), connect("a", ""),
            connect("A", ""), close("z")]);
        let mut transport = Scripted::default();
        let report = run_scenario(&scenario, &mut transport).unwrap();
        assert_eq!(report.steps_completed, 5);
        assert!(report.calls.is_empty());
        assert_eq!(transport.events, vec!["connect:λ", "connect:z", "connect:a", "connect:A",
            "close:z", "close:A", "close:a", "close:λ"]);
    }

    #[test]
    fn sequence_mismatches_stop_execution_and_cleanup_every_active_name() {
        for sequence in [None, Some(0), Some(2), Some(u64::MAX)] {
            let scenario = model(vec![connect("z", ""), connect("a", ""), call("z", 9, "r"), receive("a", "never")]);
            let mut transport = Scripted::with_replies(vec![reply(b"null", sequence, None)]);
            match run_scenario(&scenario, &mut transport).unwrap_err() {
                RunnerError::Transport { connection, detail } => {
                    assert_eq!(connection, "z");
                    assert!(detail.contains("sequence"));
                }
                other => panic!("expected correlation error, got {other:?}"),
            }
            assert_eq!(transport.events, vec!["connect:z", "connect:a", "call:z", "close:a", "close:z"]);
        }
    }

    #[test]
    fn failed_connect_is_not_tracked_and_prior_connections_are_cleaned() {
        let mut transport = Scripted { connect_failure: Some("b".into()), ..Scripted::default() };
        let scenario = model(vec![connect("z", ""), connect("a", ""), connect("b", ""), connect("later", "")]);
        assert_eq!(run_scenario(&scenario, &mut transport).unwrap_err(), transport_error("connect failed"));
        assert_eq!(transport.events, vec!["connect:z", "connect:a", "connect:b", "close:a", "close:z"]);
    }

    #[test]
    fn io_errors_pass_through_and_cleanup_errors_do_not_replace_them() {
        for operation in [call("c", 1, "r"), receive("c", "r")] {
            for original in [transport_error("operation failed"), RunnerError::Timeout { operation: "safe operation".into() },
                RunnerError::Json(Error::new(ErrorCode::TransportIncomplete, ErrorSource::Transport, 3, "incomplete"))] {
                let mut transport = Scripted::default();
                transport.replies.push_back(Err(original.clone()));
                transport.close_failures.insert("c".into(), 1);
                let scenario = model(vec![connect("c", ""), operation.clone(), close("c")]);
                assert_eq!(run_scenario(&scenario, &mut transport).unwrap_err(), original);
                assert_eq!(transport.events.last().map(String::as_str), Some("close:c"));
                assert_eq!(transport.events.len(), 3);
            }
        }
    }

    #[test]
    fn malformed_call_or_receive_replies_fail_before_later_steps() {
        for operation in [call("c", 1, "r"), receive("c", "r")] {
            let mut transport = Scripted::with_replies(vec![reply(b"[0,]", Some(1), None)]);
            let scenario = model(vec![connect("c", ""), operation, assert_step("r", "", Expectation::Exists)]);
            assert!(matches!(run_scenario(&scenario, &mut transport), Err(RunnerError::Json(_))));
            assert_eq!(transport.events.len(), 3);
            assert_eq!(transport.events.last().map(String::as_str), Some("close:c"));
        }
    }

    #[test]
    fn assertion_failure_uses_actual_step_index_and_preserves_first_error() {
        let scenario = model(vec![connect("z", ""), connect("a", ""), receive("z", "r"),
            assert_step("r", "x", equals(b"true")), receive("a", "never")]);
        let mut transport = Scripted::with_replies(vec![reply(br#"{"x":false}"#, None, None)]);
        transport.close_failures.insert("a".into(), 1);
        assert_mismatch(run_scenario(&scenario, &mut transport).map(|_| ()), 3);
        assert_eq!(transport.events, vec!["connect:z", "connect:a", "receive:z", "close:a", "close:z"]);
    }

    #[test]
    fn final_cleanup_returns_first_lexical_error_and_attempts_all_names_once() {
        let mut transport = Scripted::default();
        transport.close_failures.insert("a".into(), 1);
        transport.close_failures.insert("z".into(), 1);
        let scenario = model(vec![connect("z", ""), connect("m", ""), connect("a", "")]);
        assert_eq!(run_scenario(&scenario, &mut transport).unwrap_err(), transport_error("close failed:a"));
        assert_eq!(transport.events, vec!["connect:z", "connect:m", "connect:a", "close:a", "close:m", "close:z"]);
    }

    #[test]
    fn failed_explicit_close_is_retried_once_and_its_error_wins() {
        let scenario = model(vec![connect("z", ""), connect("a", ""), close("z"), receive("a", "never")]);
        let mut transport = Scripted::default();
        transport.close_failures.insert("z".into(), 2);
        transport.close_failures.insert("a".into(), 1);
        assert_eq!(run_scenario(&scenario, &mut transport).unwrap_err(), transport_error("close failed:z"));
        assert_eq!(transport.events, vec!["connect:z", "connect:a", "close:z", "close:a", "close:z"]);
    }

    #[test]
    fn responses_remain_owned_through_cleanup_and_drop_on_success_or_failure() {
        for fail_assertion in [false, true] {
            let bytes: Arc<[u8]> = Arc::from(b"null".as_slice());
            let weak = Arc::downgrade(&bytes);
            let mut transport = Scripted::with_replies(vec![Reply {
                sequence: None, error_code: None, body: Payload::Shared(bytes),
            }]);
            transport.watched.push(weak.clone());
            let expected = if fail_assertion { Expectation::Missing } else { Expectation::Exists };
            let scenario = model(vec![connect("c", ""), receive("c", "r"), assert_step("r", "", expected)]);
            assert_eq!(run_scenario(&scenario, &mut transport).is_err(), fail_assertion);
            assert_eq!(transport.live_at_close, vec![vec![true]]);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn internal_progress_counts_only_fully_completed_steps_and_passed_assertions() {
        let scenario = model(vec![connect("c", ""), call("c", 1, "r"),
            assert_step("r", "", Expectation::Exists), assert_step("r", "", Expectation::Missing)]);
        let mut connections = slots::<&str>(["c"].into_iter()).unwrap();
        let mut responses = slots::<Reply>(["r"].into_iter()).unwrap();
        let mut counters = planned_counters(&scenario, &mut connections).unwrap();
        let mut report = RunReport::default();
        let mut transport = Scripted::with_replies(vec![reply(b"null", Some(1), Some(1))]);
        assert_mismatch(execute(&scenario, &mut transport, &mut connections, &mut responses,
            &mut counters, &mut report), 3);
        assert_eq!((report.steps_completed, report.assertions_passed), (3, 1));
        assert_eq!(counters, vec![("", 1, 1)]);
        cleanup(&mut transport, &mut connections).unwrap();
    }

    #[test]
    fn checked_arithmetic_accepts_last_value_and_rejects_overflow_with_step() {
        assert_eq!(next_u64(0, 0, "sequence").unwrap(), 1);
        assert_eq!(next_u64(u64::MAX - 1, 9, "sequence").unwrap(), u64::MAX);
        assert_eq!(next_usize(usize::MAX - 1, 9, "counter").unwrap(), usize::MAX);
        for error in [next_u64(u64::MAX, 9, "sequence").unwrap_err(),
            next_usize(usize::MAX, 9, "counter").unwrap_err()] {
            assert!(matches!(error, RunnerError::InvalidScenario { step: Some(9), .. }));
        }
        let scenario = model(vec![connect("c", ""), call("c", 1, "r")]);
        let mut connections = slots::<&str>(["c"].into_iter()).unwrap();
        let mut responses = slots::<Reply>(["r"].into_iter()).unwrap();
        let mut counters = vec![("", 1, u64::MAX)];
        let mut transport = Scripted::with_replies(vec![reply(b"null", Some(1), None)]);
        let mut report = RunReport::default();
        assert!(matches!(execute(&scenario, &mut transport, &mut connections, &mut responses,
            &mut counters, &mut report), Err(RunnerError::InvalidScenario { step: Some(1), .. })));
        assert_eq!(report.steps_completed, 1);
        assert!(responses[0].1.is_none());
        cleanup(&mut transport, &mut connections).unwrap();
    }

    #[test]
    fn diagnostics_do_not_echo_reply_or_expectation_contents() {
        let secret = "credential-private-value";
        let body = reply(format!("\"{secret}\"").as_bytes(), None, None);
        let error = assert_reply(&body, "", &equals(b"null")).unwrap_err();
        assert!(!error.to_string().contains(secret));
        let error = assert_reply(&reply(b"null", None, None), "", &equals(format!("\"{secret}\"").as_bytes())).unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}
