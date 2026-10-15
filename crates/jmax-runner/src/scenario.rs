//! Strict versioned scenario decoding and transport-independent model validation.
//!
//! The complete source is parsed before its model is inspected, so JSON errors
//! retain their original byte offsets, including errors inside retained fragments.

use super::*;
use jmax_dom::{navigate, scalar, Node, NodeKind, Object};

/// Parse exactly one version 1 scenario and validate its execution order.
///
/// JSON fragments and decoded strings are copied into independent owned storage.
/// Unknown model fields are rejected, while arbitrary object members are allowed
/// inside `params` and `json_equals` values. Duplicate keys are rejected everywhere.
pub fn parse_scenario(input: &[u8]) -> Result<Scenario> {
    if input.len() > MAX_SCENARIO_BYTES {
        return Err(RunnerError::Json(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            MAX_SCENARIO_BYTES as u32,
            "scenario exceeds MAX_SCENARIO_BYTES",
        )));
    }

    let document = jmax_dom::build::parse(copy_payload(input, 0)?, Limits::strict_default())?;
    let root = object_fields(
        navigate::root(&document),
        &["version", "name", "timeout_ms", "steps"],
        None,
        "scenario must be an object with only version, name, timeout_ms, and steps",
    )?;
    let version = unsigned(
        required(root, "version", None, "version is required")?,
        None,
        "version must be an unsigned 32-bit integer",
    )?;
    if version != CONTRACT_VERSION {
        return Err(invalid(None, "version is not supported"));
    }
    let name = string_field(root, "name", None, "name is required and must be a string")?;
    let timeout_ms = match root.get("timeout_ms") {
        Some(value) => unsigned(value, None, "timeout_ms must be an unsigned 32-bit integer")?,
        None => DEFAULT_TIMEOUT_MS,
    };
    let steps_node = required(root, "steps", None, "steps is required")?;
    let array = navigate::array(steps_node)
        .map_err(|_| invalid(None, "steps must be an array"))?;
    if array.len() > MAX_STEPS {
        return Err(invalid(None, "steps exceeds MAX_STEPS"));
    }
    let mut steps = Vec::new();
    steps.try_reserve_exact(array.len())
        .map_err(|_| allocation_error(navigate::raw_span(steps_node).offset))?;
    for (index, node) in array.iter().enumerate() {
        steps.push(parse_step(node, index)?);
    }
    let scenario = Scenario { name, timeout_ms, steps };
    validate_scenario(&scenario)?;
    Ok(scenario)
}

/// Validate a scenario, including models created directly by Rust callers.
///
/// A response name is reserved for the whole scenario. A connection name is
/// reserved only while active. Lookup tables borrow the model and use fallible
/// vector reservations; sorting once keeps validation O(n log n) in step count.
pub fn validate_scenario(scenario: &Scenario) -> Result<()> {
    if scenario.name.is_empty() {
        return Err(invalid(None, "name must not be empty"));
    }
    if scenario.timeout_ms == 0 {
        return Err(invalid(None, "timeout_ms must be positive"));
    }
    if scenario.steps.len() > MAX_STEPS {
        return Err(invalid(None, "steps exceeds MAX_STEPS"));
    }

    let mut connections = name_states(scenario.steps.iter().filter_map(|step| match step {
        Step::Connect { name, .. } => Some(name.as_str()),
        _ => None,
    }))?;
    let mut responses = name_states(scenario.steps.iter().filter_map(|step| match step {
        Step::Call { save_as, .. } | Step::Receive { save_as, .. } => Some(save_as.as_str()),
        _ => None,
    }))?;

    for (index, step) in scenario.steps.iter().enumerate() {
        match step {
            Step::Connect { name, .. } => {
                nonempty(name, index, "connect.name must not be empty")?;
                let state = state_mut(&mut connections, name)
                    .ok_or_else(|| invalid(Some(index), "connect.name is absent from validation state"))?;
                if *state {
                    return Err(invalid(Some(index), "connect.name is already active"));
                }
                *state = true;
            }
            Step::Call { connection, params, save_as, .. } => {
                require_connection(&connections, connection, index)?;
                validate_fragment(params)?;
                save_response(&mut responses, save_as, index)?;
            }
            Step::Receive { connection, save_as } => {
                require_connection(&connections, connection, index)?;
                save_response(&mut responses, save_as, index)?;
            }
            Step::Assert { response, path, expected } => {
                nonempty(response, index, "assert.response must not be empty")?;
                if !is_active(&responses, response) {
                    return Err(invalid(Some(index), "assert.response must have been saved by an earlier step"));
                }
                jmax_path::parse::parse(path, jmax_path::PathLimits::default())
                    .map_err(|_| invalid(Some(index),
                        "assert.path must be a valid native selector path within path limits"))?;
                match expected {
                    Expectation::JsonEquals(payload) => validate_fragment(payload)?,
                    Expectation::ErrorCode(_) if !path.is_empty() => {
                        return Err(invalid(Some(index), "error_code requires an empty assert.path"));
                    }
                    _ => {}
                }
            }
            Step::Close { connection } => {
                require_connection(&connections, connection, index)?;
                let state = state_mut(&mut connections, connection)
                    .ok_or_else(|| invalid(Some(index), "close.connection must be active"))?;
                *state = false;
            }
        }
    }
    Ok(())
}

fn parse_step(node: Node<'_>, index: usize) -> Result<Step> {
    let step = Some(index);
    let wrapper = object_fields(
        node,
        &["connect", "call", "receive", "assert", "close"],
        step,
        "step must be an object with exactly one of connect, call, receive, assert, or close",
    )?;
    if wrapper.len() != 1 {
        return Err(invalid(step, "step must contain exactly one variant"));
    }
    let member = wrapper.iter().next()
        .ok_or_else(|| invalid(step, "step must contain exactly one variant"))?;
    match member.key {
        "connect" => {
            let fields = object_fields(member.value, &["name", "uri", "role"], step,
                "connect must be an object with only name, uri, and role")?;
            Ok(Step::Connect {
                name: string_field(fields, "name", step, "connect.name is required and must be a string")?,
                uri: string_field(fields, "uri", step, "connect.uri is required and must be a string")?,
                role: string_field(fields, "role", step, "connect.role is required and must be a string")?,
            })
        }
        "call" => {
            let fields = object_fields(member.value, &["connection", "endpoint", "params", "save_as"], step,
                "call must be an object with only connection, endpoint, params, and save_as")?;
            Ok(Step::Call {
                connection: string_field(fields, "connection", step, "call.connection is required and must be a string")?,
                endpoint: unsigned(required(fields, "endpoint", step, "call.endpoint is required")?, step,
                    "call.endpoint must be an unsigned 32-bit integer")?,
                params: fragment(required(fields, "params", step, "call.params is required")?)?,
                save_as: string_field(fields, "save_as", step, "call.save_as is required and must be a string")?,
            })
        }
        "receive" => {
            let fields = object_fields(member.value, &["connection", "save_as"], step,
                "receive must be an object with only connection and save_as")?;
            Ok(Step::Receive {
                connection: string_field(fields, "connection", step, "receive.connection is required and must be a string")?,
                save_as: string_field(fields, "save_as", step, "receive.save_as is required and must be a string")?,
            })
        }
        "assert" => {
            let fields = object_fields(member.value, &["response", "path", "expected"], step,
                "assert must be an object with only response, path, and expected")?;
            Ok(Step::Assert {
                response: string_field(fields, "response", step, "assert.response is required and must be a string")?,
                path: string_field(fields, "path", step, "assert.path is required and must be a string")?,
                expected: parse_expectation(required(fields, "expected", step, "assert.expected is required")?, index)?,
            })
        }
        "close" => {
            let fields = object_fields(member.value, &["connection"], step,
                "close must be an object with only connection")?;
            Ok(Step::Close {
                connection: string_field(fields, "connection", step, "close.connection is required and must be a string")?,
            })
        }
        _ => Err(invalid(step, "step variant is not supported")),
    }
}

fn parse_expectation(node: Node<'_>, index: usize) -> Result<Expectation> {
    let step = Some(index);
    let fields = object_fields(node, &["exists", "missing", "json_equals", "error_code"], step,
        "expected must be an object with exactly one of exists, missing, json_equals, or error_code")?;
    if fields.len() != 1 {
        return Err(invalid(step, "expected must contain exactly one expectation"));
    }
    let member = fields.iter().next()
        .ok_or_else(|| invalid(step, "expected must contain exactly one expectation"))?;
    match member.key {
        "exists" | "missing" => {
            if scalar::boolean(member.value).ok() != Some(true) {
                return Err(invalid(step, "exists and missing expectation flags must be true"));
            }
            Ok(if member.key == "exists" { Expectation::Exists } else { Expectation::Missing })
        }
        "json_equals" => Ok(Expectation::JsonEquals(fragment(member.value)?)),
        "error_code" => Ok(Expectation::ErrorCode(unsigned(member.value, step,
            "expected.error_code must be an unsigned 32-bit integer")?)),
        _ => Err(invalid(step, "expectation variant is not supported")),
    }
}

fn object_fields<'a>(
    node: Node<'a>,
    allowed: &[&str],
    step: Option<usize>,
    detail: &'static str,
) -> Result<Object<'a>> {
    let object = navigate::object(node).map_err(|_| invalid(step, detail))?;
    if object.iter().any(|member| !allowed.contains(&member.key)) {
        return Err(invalid(step, detail));
    }
    Ok(object)
}

fn required<'a>(
    object: Object<'a>,
    key: &str,
    step: Option<usize>,
    detail: &'static str,
) -> Result<Node<'a>> {
    object.get(key).ok_or_else(|| invalid(step, detail))
}

fn string_field(
    object: Object<'_>,
    key: &str,
    step: Option<usize>,
    detail: &'static str,
) -> Result<String> {
    let node = required(object, key, step, detail)?;
    let text = scalar::string(node).map_err(|_| invalid(step, detail))?;
    let mut owned = String::new();
    owned.try_reserve_exact(text.len())
        .map_err(|_| allocation_error(navigate::raw_span(node).offset))?;
    owned.push_str(text);
    Ok(owned)
}

fn unsigned(node: Node<'_>, step: Option<usize>, detail: &'static str) -> Result<u32> {
    if navigate::kind(node) != NodeKind::Number {
        return Err(invalid(step, detail));
    }
    let bytes = scalar::raw(node)?;
    let mut value = 0_u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(invalid(step, detail));
        }
        value = value.checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(|| invalid(step, detail))?;
    }
    // DOM number tokens are nonempty and already obey strict JSON grammar.
    Ok(value)
}

fn fragment(node: Node<'_>) -> Result<Payload> {
    copy_payload(scalar::raw(node)?, navigate::raw_span(node).offset)
}

fn copy_payload(bytes: &[u8], offset: u32) -> Result<Payload> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(bytes.len()).map_err(|_| allocation_error(offset))?;
    owned.extend_from_slice(bytes);
    Ok(Payload::from_vec(owned))
}

fn validate_fragment(payload: &Payload) -> Result<()> {
    jmax_dom::build::parse(payload.clone(), Limits::strict_default())?;
    Ok(())
}

// Both tables store borrowed names and a flag. Connection flags mean active;
// response flags mean saved at an earlier point, and are never reset.
fn name_states<'a>(names: impl Iterator<Item = &'a str>) -> Result<Vec<(&'a str, bool)>> {
    let mut states = Vec::new();
    for name in names {
        states.try_reserve(1).map_err(|_| allocation_error(0))?;
        states.push((name, false));
    }
    states.sort_unstable_by(|left, right| left.0.cmp(right.0));
    states.dedup_by(|left, right| left.0 == right.0);
    Ok(states)
}

fn state_mut<'a>(states: &'a mut [(&str, bool)], name: &str) -> Option<&'a mut bool> {
    let index = states.binary_search_by(|entry| entry.0.cmp(name)).ok()?;
    Some(&mut states[index].1)
}

fn is_active(states: &[(&str, bool)], name: &str) -> bool {
    states.binary_search_by(|entry| entry.0.cmp(name))
        .ok().map(|index| states[index].1).unwrap_or(false)
}

fn nonempty(name: &str, index: usize, detail: &'static str) -> Result<()> {
    if name.is_empty() { Err(invalid(Some(index), detail)) } else { Ok(()) }
}

fn require_connection(states: &[(&str, bool)], name: &str, index: usize) -> Result<()> {
    nonempty(name, index, "connection must not be empty")?;
    if !is_active(states, name) {
        return Err(invalid(Some(index), "connection must be active before this step"));
    }
    Ok(())
}

fn save_response(states: &mut [(&str, bool)], name: &str, index: usize) -> Result<()> {
    nonempty(name, index, "save_as must not be empty")?;
    let saved = state_mut(states, name)
        .ok_or_else(|| invalid(Some(index), "save_as is absent from validation state"))?;
    if *saved {
        return Err(invalid(Some(index), "save_as must be unique throughout the scenario"));
    }
    *saved = true;
    Ok(())
}

fn allocation_error(offset: u32) -> RunnerError {
    RunnerError::Json(Error::new(
        ErrorCode::BudgetExhausted, ErrorSource::Decode, offset, "scenario allocation failed",
    ))
}

fn invalid(step: Option<usize>, detail: &'static str) -> RunnerError {
    let mut owned = String::new();
    if owned.try_reserve_exact(detail.len()).is_err() {
        return allocation_error(0);
    }
    owned.push_str(detail);
    RunnerError::InvalidScenario { step, detail: owned }
}
