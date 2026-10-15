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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::ToString, vec};

    const EMPTY: &[u8] = br#"{"version":1,"name":"test","steps":[]}"#;
    const CONNECT: &str = r#"{"connect":{"name":"c","uri":"ws://localhost","role":""}}"#;
    const RECEIVE: &str = r#"{"receive":{"connection":"c","save_as":"r"}}"#;

    fn parse_steps(steps: &str) -> Result<Scenario> {
        parse_scenario(format!(r#"{{"version":1,"name":"test","steps":[{steps}]}}"#).as_bytes())
    }

    fn model(steps: Vec<Step>) -> Scenario {
        Scenario { name: "test".into(), timeout_ms: DEFAULT_TIMEOUT_MS, steps }
    }

    fn connect(name: &str) -> Step {
        Step::Connect { name: name.into(), uri: "custom://transport".into(), role: String::new() }
    }

    fn receive(connection: &str, save_as: &str) -> Step {
        Step::Receive { connection: connection.into(), save_as: save_as.into() }
    }

    fn call(connection: &str, save_as: &str, params: &[u8]) -> Step {
        Step::Call {
            connection: connection.into(), endpoint: 0,
            params: Payload::from_slice(params), save_as: save_as.into(),
        }
    }

    fn assertion(response: &str, path: &str, expected: Expectation) -> Step {
        Step::Assert { response: response.into(), path: path.into(), expected }
    }

    fn assert_invalid<T: core::fmt::Debug>(result: Result<T>, expected_step: Option<usize>) -> String {
        match result.unwrap_err() {
            RunnerError::InvalidScenario { step, detail } => {
                assert_eq!(step, expected_step);
                assert!(!detail.is_empty());
                detail
            }
            error => panic!("expected model error, received {error:?}"),
        }
    }

    fn assert_json<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) -> Error {
        match result.unwrap_err() {
            RunnerError::Json(error) => { assert_eq!(error.code, code); error }
            error => panic!("expected JSON error, received {error:?}"),
        }
    }

    #[test]
    fn minimal_scenario_defaults_and_optional_final_closes() {
        let parsed = parse_scenario(EMPTY).unwrap();
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(parsed.steps.is_empty());
        assert!(validate_scenario(&parsed).is_ok());
        assert!(parse_steps(CONNECT).is_ok());
        assert!(validate_scenario(&model(vec![connect("c")])).is_ok());
    }

    #[test]
    fn every_step_and_expectation_variant() {
        let parsed = parse_steps(&format!(r#"{CONNECT},
            {{"call":{{"connection":"c","endpoint":1,"params":{{}},"save_as":"a"}}}},
            {RECEIVE},
            {{"assert":{{"response":"a","path":"status","expected":{{"json_equals":"ok"}}}}}},
            {{"assert":{{"response":"r","path":"","expected":{{"exists":true}}}}}},
            {{"assert":{{"response":"r","path":"absent","expected":{{"missing":true}}}}}},
            {{"assert":{{"response":"r","path":"","expected":{{"error_code":4294967295}}}}}},
            {{"close":{{"connection":"c"}}}}"#)).unwrap();
        assert_eq!(parsed.steps.len(), 8);
        assert!(matches!(&parsed.steps[0], Step::Connect { role, .. } if role.is_empty()));
        assert!(matches!(&parsed.steps[1], Step::Call { endpoint: 1, params, .. } if params.as_slice() == b"{}"));
        assert!(matches!(&parsed.steps[2], Step::Receive { save_as, .. } if save_as == "r"));
        assert!(matches!(&parsed.steps[3], Step::Assert { expected: Expectation::JsonEquals(value), .. }
            if value.as_slice() == br#""ok""#));
        assert!(matches!(&parsed.steps[4], Step::Assert { expected: Expectation::Exists, .. }));
        assert!(matches!(&parsed.steps[5], Step::Assert { expected: Expectation::Missing, .. }));
        assert!(matches!(&parsed.steps[6], Step::Assert { expected: Expectation::ErrorCode(u32::MAX), .. }));
        assert!(matches!(&parsed.steps[7], Step::Close { connection } if connection == "c"));
    }

    #[test]
    fn decoded_keys_strings_surrogates_and_owned_fragments() {
        let mut input = br#"{"ver\u0073ion":1,"name":"A\uD83D\uDE80\n\"\\","steps":[
            {"connect":{"name":"\u0063","uri":"custom://host","role":"\u03BB"}},
            {"call":{"connection":"c","endpoint":7,"params":{ "x" : [null,true,1.00,"\u0061"] },"save_as":"r"}},
            {"assert":{"response":"r","path":"[\"a.b\"]","expected":{"json_equals":[1e2,{"z":"\uD834\uDD1E"}]}}}
        ]}"#.to_vec();
        let parsed = parse_scenario(&input).unwrap();
        input.fill(b'x');
        drop(input);
        assert_eq!(parsed.name, "A🚀\n\"\\");
        assert!(matches!(&parsed.steps[0], Step::Connect { name, role, .. } if name == "c" && role == "λ"));
        match &parsed.steps[1] {
            Step::Call { params, .. } => assert_eq!(params.as_slice(), br#"{ "x" : [null,true,1.00,"\u0061"] }"#),
            _ => panic!("expected call"),
        }
        match &parsed.steps[2] {
            Step::Assert { path, expected: Expectation::JsonEquals(value), .. } => {
                assert_eq!(path, r#"["a.b"]"#);
                assert_eq!(value.as_slice(), br#"[1e2,{"z":"\uD834\uDD1E"}]"#);
            }
            _ => panic!("expected JSON assertion"),
        }
        assert!(validate_scenario(&parsed).is_ok());
    }

    #[test]
    fn fragments_accept_every_json_kind_and_preserve_number_lexemes() {
        for json in ["null", "false", "true", "0", "-0", "1.0", "1e+999", r#""text""#, "[]", "{}"] {
            let parsed = parse_steps(&format!(r#"{CONNECT},
                {{"call":{{"connection":"c","endpoint":0,"params":{json},"save_as":"r"}}}},
                {{"assert":{{"response":"r","path":"","expected":{{"json_equals":{json}}}}}}"#)).unwrap();
            match (&parsed.steps[1], &parsed.steps[2]) {
                (Step::Call { params, .. }, Step::Assert { expected: Expectation::JsonEquals(expected), .. }) => {
                    assert_eq!(params.as_slice(), json.as_bytes());
                    assert_eq!(expected.as_slice(), json.as_bytes());
                }
                _ => panic!("expected retained fragments"),
            }
        }
    }

    #[test]
    fn unsigned_integer_boundaries_and_timeout_override() {
        for timeout in [1, u32::MAX] {
            let input = format!(r#"{{"steps":[],"timeout_ms":{timeout},"name":"test","version":1}}"#);
            assert_eq!(parse_scenario(input.as_bytes()).unwrap().timeout_ms, timeout);
        }
        for endpoint in [0, u32::MAX] {
            let parsed = parse_steps(&format!(r#"{CONNECT},
                {{"call":{{"connection":"c","endpoint":{endpoint},"params":null,"save_as":"r"}}}}"#)).unwrap();
            assert!(matches!(&parsed.steps[1], Step::Call { endpoint: value, .. } if *value == endpoint));
        }
        for code in [0, u32::MAX] {
            let parsed = parse_steps(&format!(r#"{CONNECT},{RECEIVE},
                {{"assert":{{"response":"r","path":"","expected":{{"error_code":{code}}}}}}"#)).unwrap();
            assert!(matches!(&parsed.steps[2], Step::Assert { expected: Expectation::ErrorCode(value), .. } if *value == code));
        }
    }

    #[test]
    fn integer_fields_reject_sign_fraction_exponent_overflow_and_wrong_types() {
        for value in ["-1", "-0", "1.0", "1e0", "4294967296", "18446744073709551616", "null", "true", r#""1""#, "{}", "[]"] {
            assert_invalid(parse_scenario(format!(r#"{{"version":{value},"name":"x","steps":[]}}"#).as_bytes()), None);
            assert_invalid(parse_scenario(format!(r#"{{"version":1,"timeout_ms":{value},"name":"x","steps":[]}}"#).as_bytes()), None);
            assert_invalid(parse_steps(&format!(r#"{CONNECT},
                {{"call":{{"connection":"c","endpoint":{value},"params":null,"save_as":"r"}}}}"#)), Some(1));
            assert_invalid(parse_steps(&format!(r#"{CONNECT},{RECEIVE},
                {{"assert":{{"response":"r","path":"","expected":{{"error_code":{value}}}}}}"#)), Some(2));
        }
        for version in [0, 2, u32::MAX] {
            assert_invalid(parse_scenario(format!(r#"{{"version":{version},"name":"x","steps":[]}}"#).as_bytes()), None);
        }
        assert_invalid(parse_scenario(br#"{"version":1,"name":"x","timeout_ms":0,"steps":[]}"#), None);
    }

    #[test]
    fn top_level_required_fields_types_and_unknown_keys() {
        for input in [
            "null", "[]", "{}", r#"{"version":1,"steps":[]}"#,
            r#"{"name":"x","steps":[]}"#, r#"{"version":1,"name":"x"}"#,
            r#"{"version":1,"name":null,"steps":[]}"#,
            r#"{"version":1,"name":"x","steps":{}}"#,
            r#"{"version":1,"name":"x","steps":[],"extra":0}"#,
            r#"{"version":1,"name":"","steps":[]}"#,
        ] {
            assert_invalid(parse_scenario(input.as_bytes()), None);
        }
    }

    #[test]
    fn variant_wrapper_requires_exactly_one_known_variant() {
        for step in ["null", "[]", "{}", r#"{"other":{}}"#,
            r#"{"connect":{},"close":{}}"#, r#"{"close":{},"extra":0}"#] {
            assert_invalid(parse_steps(step), Some(0));
        }
    }

    #[test]
    fn variant_fields_are_required_typed_and_closed() {
        for step in [
            r#"{"connect":null}"#, r#"{"connect":{}}"#,
            r#"{"connect":{"name":"c","uri":"x"}}"#,
            r#"{"connect":{"name":"c","role":""}}"#,
            r#"{"connect":{"uri":"x","role":""}}"#,
            r#"{"connect":{"name":"c","uri":null,"role":""}}"#,
            r#"{"connect":{"name":"c","uri":"x","role":false}}"#,
            r#"{"connect":{"name":"c","uri":"x","role":"","extra":0}}"#,
            r#"{"call":{}}"#, r#"{"call":{"connection":"c","endpoint":1,"save_as":"r"}}"#,
            r#"{"call":{"connection":"c","params":{},"save_as":"r"}}"#,
            r#"{"call":{"endpoint":1,"params":{},"save_as":"r"}}"#,
            r#"{"call":{"connection":"c","endpoint":1,"params":{}}}"#,
            r#"{"call":{"connection":"c","endpoint":1,"params":{},"save_as":"r","extra":0}}"#,
            r#"{"receive":{"connection":"c"}}"#, r#"{"receive":{"save_as":"r"}}"#,
            r#"{"receive":{"connection":"c","save_as":0}}"#,
            r#"{"receive":{"connection":"c","save_as":"r","extra":0}}"#,
            r#"{"assert":{"response":"r","path":""}}"#,
            r#"{"assert":{"response":"r","expected":{"exists":true}}}"#,
            r#"{"assert":{"path":"","expected":{"exists":true}}}"#,
            r#"{"assert":{"response":"r","path":null,"expected":{"exists":true}}}"#,
            r#"{"assert":{"response":"r","path":"","expected":{"exists":true},"extra":0}}"#,
            r#"{"close":{}}"#, r#"{"close":{"connection":false}}"#,
            r#"{"close":{"connection":"c","extra":0}}"#,
        ] {
            assert_invalid(parse_steps(&format!("{CONNECT},{RECEIVE},{step}")), Some(2));
        }
    }

    #[test]
    fn expectation_shape_and_flags_are_strict() {
        for expected in ["null", "[]", "{}", r#"{"exists":false}"#, r#"{"missing":false}"#,
            r#"{"exists":1}"#, r#"{"missing":"true"}"#, r#"{"exists":null}"#,
            r#"{"exists":true,"missing":true}"#, r#"{"unknown":true}"#] {
            assert_invalid(parse_steps(&format!(r#"{CONNECT},{RECEIVE},
                {{"assert":{{"response":"r","path":"","expected":{expected}}}}}"#)), Some(2));
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_after_decoding_at_every_level() {
        for input in [
            r#"{"version":1,"name":"x","na\u006de":"y","steps":[]}"#.to_string(),
            format!(r#"{{"version":1,"name":"x","steps":[{{"connect":{{"name":"c","name":"d","uri":"x","role":""}}}}]}}"#),
            format!(r#"{{"version":1,"name":"x","steps":[{{"close":{{}},"close":{{}}}}]}}"#),
            format!(r#"{{"version":1,"name":"x","steps":[{CONNECT},{{"call":{{"connection":"c","endpoint":1,"params":{{"a":1,"\u0061":2}},"save_as":"r"}}}}]}}"#),
            format!(r#"{{"version":1,"name":"x","steps":[{CONNECT},{RECEIVE},{{"assert":{{"response":"r","path":"","expected":{{"exists":true,"exists":true}}}}}}]}}"#),
            format!(r#"{{"version":1,"name":"x","steps":[{CONNECT},{RECEIVE},{{"assert":{{"response":"r","path":"","expected":{{"json_equals":[{{"a":0,"a":1}}]}}}}}}]}}"#),
        ] {
            let error = assert_json(parse_scenario(input.as_bytes()), ErrorCode::DuplicateObjectKey);
            assert!((error.byte_offset as usize) < input.len());
        }
    }

    #[test]
    fn complete_json_is_validated_before_model_and_offsets_are_original() {
        let mut trailing = EMPTY.to_vec();
        trailing.extend_from_slice(b" \n false");
        let error = assert_json(parse_scenario(&trailing), ErrorCode::TrailingContent);
        assert_eq!(error.byte_offset as usize, EMPTY.len() + 3);
        let malformed = br#"{"version":1,"name":"x","steps":[{"call":{"params":[0,]}}]}"#;
        assert!(matches!(parse_scenario(malformed), Err(RunnerError::Json(_))));
        let escaped = br#"{"version":1,"name":"x","steps":[],"unknown":"\q"}"#;
        let document_error = jmax_dom::build::parse(Payload::from_slice(escaped), Limits::strict_default()).unwrap_err();
        match parse_scenario(escaped).unwrap_err() {
            RunnerError::Json(error) => assert_eq!(error, document_error),
            error => panic!("expected original JSON error: {error:?}"),
        }
    }

    #[test]
    fn rejects_non_json_syntax_invalid_numbers_and_incomplete_input() {
        for input in [
            "", " ", "{", r#"{"version":1,"name":"x","steps":[}"#,
            r#"{"version":01,"name":"x","steps":[]}"#,
            r#"{"version":+1,"name":"x","steps":[]}"#,
            r#"{"version":1.,"name":"x","steps":[]}"#,
            r#"{"version":1e,"name":"x","steps":[]}"#,
            r#"{"version":NaN,"name":"x","steps":[]}"#,
            r#"{"version":1,"name":"x","steps":[],}"#,
            r#"{/*comment*/"version":1,"name":"x","steps":[]}"#,
            r#"{'version':1,'name':'x','steps':[]}"#,
        ] {
            assert!(matches!(parse_scenario(input.as_bytes()), Err(RunnerError::Json(_))), "{input}");
        }
        for text in [r#""\uD800""#, r#""\uDC00""#, r#""\uD800\u0041""#,
            r#""\q""#, r#""\uZZZZ""#, "\"line\nbreak\""] {
            let input = format!(r#"{{"version":1,"name":{text},"steps":[]}}"#);
            assert!(matches!(parse_scenario(input.as_bytes()), Err(RunnerError::Json(_))));
        }
        let mut invalid_utf8 = br#"{"version":1,"name":""#.to_vec();
        invalid_utf8.push(0xff);
        invalid_utf8.extend_from_slice(br#"","steps":[]}"#);
        assert_json(parse_scenario(&invalid_utf8), ErrorCode::InvalidUtf8);
    }

    #[test]
    fn byte_limit_is_inclusive() {
        let mut input = EMPTY.to_vec();
        input.resize(MAX_SCENARIO_BYTES, b' ');
        assert!(parse_scenario(&input).is_ok());
        input.push(b' ');
        let error = assert_json(parse_scenario(&input), ErrorCode::PayloadTooLarge);
        assert_eq!(error.byte_offset as usize, MAX_SCENARIO_BYTES);
    }

    #[test]
    fn depth_and_string_limits_are_enforced() {
        let limit = Limits::strict_default();
        let nested = format!("{}0{}", "[".repeat(limit.max_depth as usize + 1), "]".repeat(limit.max_depth as usize + 1));
        let input = format!(r#"{{"version":1,"name":"x","steps":[],"unknown":{nested}}}"#);
        assert_json(parse_scenario(input.as_bytes()), ErrorCode::DepthLimitExceeded);
        let input = format!(r#"{{"version":1,"name":"{}","steps":[]}}"#, "x".repeat(limit.max_string_bytes as usize + 1));
        assert!(matches!(parse_scenario(input.as_bytes()), Err(RunnerError::Json(_))));
    }

    #[test]
    fn step_limit_is_inclusive_for_direct_models_and_enforced_by_parser() {
        let mut steps = Vec::with_capacity(MAX_STEPS + 1);
        for index in 0..MAX_STEPS {
            steps.push(if index % 2 == 0 { connect("c") }
                else { Step::Close { connection: "c".into() } });
        }
        let mut scenario = model(steps);
        assert!(validate_scenario(&scenario).is_ok());
        scenario.steps.push(connect("extra"));
        assert_invalid(validate_scenario(&scenario), None);

        let close = r#"{"close":{"connection":"c"}}"#;
        let mut input = String::from(r#"{"version":1,"name":"x","steps":["#);
        for index in 0..=MAX_STEPS {
            if index != 0 { input.push(','); }
            input.push_str(close);
        }
        input.push_str("]}");
        assert_invalid(parse_scenario(input.as_bytes()), None);
    }

    #[test]
    fn connection_names_can_be_reused_after_close_and_responses_survive_close() {
        let scenario = model(vec![
            connect("c"), receive("c", "first"), Step::Close { connection: "c".into() },
            assertion("first", "", Expectation::Exists), connect("c"),
            call("c", "second", b"null"), Step::Close { connection: "c".into() },
            assertion("second", "", Expectation::Missing),
        ]);
        assert!(validate_scenario(&scenario).is_ok());
    }

    #[test]
    fn parsed_models_enforce_ordering_and_decoded_name_identity() {
        assert_invalid(parse_steps(&format!("{RECEIVE},{CONNECT}")), Some(0));
        assert_invalid(parse_steps(&format!("{CONNECT},{CONNECT}")), Some(1));
        assert_invalid(parse_steps(&format!("{CONNECT},{RECEIVE},{RECEIVE}")), Some(2));
        assert_invalid(parse_steps(&format!(r#"{CONNECT},
            {{"connect":{{"name":"\u0063","uri":"x","role":""}}}}"#)), Some(1));
        let close = r#"{"close":{"connection":"c"}}"#;
        assert!(parse_steps(&format!("{CONNECT},{close},{CONNECT}")).is_ok());
        assert_invalid(parse_steps(&format!(r#"{CONNECT},
            {{"assert":{{"response":"r","path":"","expected":{{"missing":true}}}}}},
            {RECEIVE}"#)), Some(1));
    }

    #[test]
    fn validation_state_handles_many_names_in_reverse_insertion_order() {
        let mut steps = Vec::new();
        for index in (0..64).rev() {
            steps.push(connect(&format!("c{index:02}")));
        }
        for index in 0..64 {
            steps.push(receive(&format!("c{index:02}"), &format!("r{index:02}")));
        }
        for index in (0..64).rev() {
            steps.push(Step::Close { connection: format!("c{index:02}") });
            steps.push(assertion(&format!("r{index:02}"), "", Expectation::Exists));
        }
        assert!(validate_scenario(&model(steps)).is_ok());
    }

    #[test]
    fn duplicate_active_connections_and_inactive_uses_are_rejected() {
        assert_invalid(validate_scenario(&model(vec![connect("c"), connect("c")])), Some(1));
        for operation in [call("c", "r", b"null"), receive("c", "r"), Step::Close { connection: "c".into() }] {
            assert_invalid(validate_scenario(&model(vec![operation.clone(), connect("c")])), Some(0));
            assert_invalid(validate_scenario(&model(vec![connect("c"), Step::Close { connection: "c".into() }, operation])), Some(2));
        }
    }

    #[test]
    fn response_names_are_global_and_references_must_be_earlier() {
        for first in [receive("c", "r"), call("c", "r", b"{}")] {
            for second in [receive("c", "r"), call("c", "r", b"{}")] {
                let scenario = model(vec![connect("c"), first.clone(), Step::Close { connection: "c".into() }, connect("c"), second]);
                assert_invalid(validate_scenario(&scenario), Some(4));
            }
        }
        assert_invalid(validate_scenario(&model(vec![
            connect("c"), assertion("r", "", Expectation::Exists), receive("c", "r"),
        ])), Some(1));
        assert_invalid(validate_scenario(&model(vec![assertion("absent", "", Expectation::Missing)])), Some(0));
    }

    #[test]
    fn connection_and_response_names_use_distinct_case_sensitive_namespaces() {
        assert!(validate_scenario(&model(vec![
            connect("c"), connect("C"), receive("c", "c"), receive("C", "C"),
            assertion("c", "", Expectation::Exists), assertion("C", "", Expectation::Exists),
        ])).is_ok());
        assert_invalid(validate_scenario(&model(vec![connect("c"), receive("C", "r")])), Some(1));
        assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("R", "", Expectation::Exists)])), Some(2));
    }

    #[test]
    fn all_empty_name_positions_and_zero_timeout_are_rejected_directly() {
        let mut scenario = model(Vec::new());
        scenario.name.clear();
        assert_invalid(validate_scenario(&scenario), None);
        scenario.name.push('x');
        scenario.timeout_ms = 0;
        assert_invalid(validate_scenario(&scenario), None);
        assert_invalid(validate_scenario(&model(vec![connect("")])), Some(0));
        for operation in [call("", "r", b"null"), receive("", "r"), Step::Close { connection: String::new() },
            call("c", "", b"null"), receive("c", "")] {
            assert_invalid(validate_scenario(&model(vec![connect("c"), operation])), Some(1));
        }
        assert_invalid(validate_scenario(&model(vec![assertion("", "", Expectation::Exists)])), Some(0));
    }

    #[test]
    fn uri_validation_is_left_to_transports_and_roles_can_be_empty() {
        for uri in ["", "not a URI", "custom://supported-by-a-transport"] {
            let scenario = model(vec![Step::Connect { name: "c".into(), uri: uri.into(), role: String::new() }]);
            assert!(validate_scenario(&scenario).is_ok());
        }
        let parsed = parse_steps(r#"{"connect":{"name":" ","uri":"","role":""}}"#).unwrap();
        assert!(matches!(&parsed.steps[0], Step::Connect { name, uri, role } if name == " " && uri.is_empty() && role.is_empty()));
    }

    #[test]
    fn native_selector_paths_accept_roots_keys_escapes_and_indices() {
        for path in ["", "$", "a", "$.a[0].b", "[0][1]", r#"["a.b"][2]"#, r"a\.b", r#"[""]"#, "λ[4294967295]"] {
            assert!(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
                assertion("r", path, Expectation::Exists)])).is_ok(), "{path}");
        }
    }

    #[test]
    fn native_selector_paths_reject_malformed_and_extended_syntax() {
        for path in [".", "a.", "a..b", "a[", "a[-1]", "a[01]", "a[4294967296]", "a[ 0]", "a[*]", "*", "a[?(@.x==1)]", " a", "a ", r#"["\uD800"]"#] {
            assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
                assertion("r", path, Expectation::Exists)])), Some(2));
        }
        let path = "a".repeat(jmax_path::PathLimits::default().max_path_bytes as usize + 1);
        assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &path, Expectation::Exists)])), Some(2));
    }

    #[test]
    fn native_selector_key_and_segment_limits_are_inclusive() {
        let limits = jmax_path::PathLimits::default();
        let mut key = "a".repeat(limits.max_key_bytes as usize);
        assert!(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &key, Expectation::Exists)])).is_ok());
        key.push('a');
        assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &key, Expectation::Exists)])), Some(2));
        let mut path = String::from("a");
        for _ in 1..limits.max_segments { path.push_str(".a"); }
        assert!(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &path, Expectation::Exists)])).is_ok());
        path.push_str(".a");
        assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &path, Expectation::Exists)])), Some(2));
    }

    #[test]
    fn error_code_expectations_require_a_literally_empty_path() {
        for path in ["$", "status"] {
            assert_invalid(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
                assertion("r", path, Expectation::ErrorCode(0))])), Some(2));
        }
        assert!(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", "", Expectation::ErrorCode(u32::MAX))])).is_ok());
    }

    #[test]
    fn directly_constructed_fragments_are_strictly_validated() {
        for bytes in [b"".as_slice(), b"null true", b"[1,]", b"{\"a\":1,\"a\":2}", b"\"\\uD800\"", b"01", b"\"\xff\""] {
            assert!(matches!(validate_scenario(&model(vec![connect("c"), call("c", "r", bytes)])), Err(RunnerError::Json(_))));
            assert!(matches!(validate_scenario(&model(vec![connect("c"), receive("c", "r"),
                assertion("r", "", Expectation::JsonEquals(Payload::from_slice(bytes)))])), Err(RunnerError::Json(_))));
        }
        assert_json(validate_scenario(&model(vec![connect("c"), call("c", "r", b"{\"a\":0,\"a\":1}")])), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn diagnostics_do_not_echo_supplied_values() {
        let secret = "private-credential-value";
        let error = validate_scenario(&model(vec![receive(secret, "r")])).unwrap_err();
        assert!(!error.to_string().contains(secret));
        let error = validate_scenario(&model(vec![connect(secret), connect(secret)])).unwrap_err();
        assert!(!error.to_string().contains(secret));
        let input = format!(r#"{{"version":1,"name":"x","steps":[],"{secret}":"{secret}"}}"#);
        assert!(!parse_scenario(input.as_bytes()).unwrap_err().to_string().contains(secret));
        let error = validate_scenario(&model(vec![connect("c"), receive("c", "r"),
            assertion("r", &format!("{secret}..x"), Expectation::Exists)])).unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}
