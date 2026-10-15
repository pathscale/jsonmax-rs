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
