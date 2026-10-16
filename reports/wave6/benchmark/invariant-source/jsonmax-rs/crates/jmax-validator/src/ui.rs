//! List, native line, and terminal-oriented adapters for the validator.

use super::*;
use std::io::ErrorKind;

const HEADER: &[u8] = b"service\tendpoint\tcode\tdescription\n";

/// Returns owned endpoint rows in stable UTF-8 byte order.
pub fn rows(model: &ValidatorModel) -> Vec<EndpointRow> {
    let mut result = Vec::new();
    for service in &model.services {
        for endpoint in &service.endpoints {
            result.push(EndpointRow {
                key: EndpointKey { service_id: service.id, code: endpoint.code },
                service: service.name.clone(),
                endpoint: endpoint.name.clone(),
                description: endpoint.description.clone(),
            });
        }
    }
    result.sort_by(|left, right| {
        left.service.as_bytes().cmp(right.service.as_bytes())
            .then_with(|| left.endpoint.as_bytes().cmp(right.endpoint.as_bytes()))
            .then_with(|| left.key.code.cmp(&right.key.code))
    });
    result
}

fn write_cell(output: &mut dyn Write, value: &str) -> ValidatorResult<()> {
    let io_error = |source| ValidatorError::Io {
        operation: "write stdout", path: PathBuf::from("<stdout>"), source,
    };
    for ch in value.chars() {
        match ch {
            '\\' => output.write_all(b"\\\\").map_err(io_error)?,
            '\t' => output.write_all(b"\\t").map_err(io_error)?,
            '\r' => output.write_all(b"\\r").map_err(io_error)?,
            '\n' => output.write_all(b"\\n").map_err(io_error)?,
            c if c.is_control() => {
                write!(output, "\\u{{{:x}}}", c as u32).map_err(io_error)?;
            }
            c => {
                let mut encoded = [0; 4];
                output.write_all(c.encode_utf8(&mut encoded).as_bytes()).map_err(io_error)?;
            }
        }
    }
    Ok(())
}

fn write_io(output: &mut dyn Write, bytes: &[u8]) -> ValidatorResult<()> {
    output.write_all(bytes).map_err(|source| ValidatorError::Io {
        operation: "write stdout", path: PathBuf::from("<stdout>"), source,
    })
}

/// Writes an unstyled, escaped endpoint table.
pub fn render_list(model: &ValidatorModel, output: &mut dyn Write) -> ValidatorResult<()> {
    write_io(output, HEADER)?;
    for row in rows(model) {
        write_cell(output, &row.service)?;
        write_io(output, b"\t")?;
        write_cell(output, &row.endpoint)?;
        write_io(output, b"\t")?;
        write_io(output, row.key.code.to_string().as_bytes())?;
        write_io(output, b"\t")?;
        write_cell(output, &row.description)?;
        write_io(output, b"\n")?;
    }
    Ok(())
}

fn semantic_error(code: ErrorCode, detail: &'static str) -> ValidatorError {
    ValidatorError::Validation(AbiError::new(code, ErrorSource::Decode, 0, detail))
}

fn escaped(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        let repr = match ch {
            '\\' => "\\\\".to_owned(),
            '\n' => "\\n".to_owned(),
            '\r' => "\\r".to_owned(),
            '\t' => "\\t".to_owned(),
            c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
            c => c.to_string(),
        };
        if output.len().saturating_add(repr.len()) > maximum { break; }
        output.push_str(&repr);
    }
    output
}

fn report_error(output: &mut dyn Write, error: &ValidatorError, limits: Limits) -> ValidatorResult<()> {
    let message = escaped(&error.to_string(), limits.max_string_bytes as usize);
    write_line(output, &format!("error: {message}"))
}

fn write_line(output: &mut dyn Write, text: &str) -> ValidatorResult<()> {
    write_io(output, text.as_bytes())?;
    write_io(output, b"\n")
}

/// Reads one UTF-8 input line without retaining more than the configured cap.
/// An oversized line is drained through its newline so later commands remain aligned.
fn read_bounded_line(input: &mut dyn BufRead, limit: usize) -> ValidatorResult<Option<String>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = input.fill_buf().map_err(|source| ValidatorError::Io {
            operation: "read stdin", path: PathBuf::from("<stdin>"), source,
        })?;
        if available.is_empty() {
            if bytes.is_empty() && !oversized { return Ok(None); }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content_len = newline.map_or(consumed, |index| index);
        if !oversized {
            let room = limit.saturating_sub(bytes.len());
            let take = room.min(content_len);
            bytes.extend_from_slice(&available[..take]);
            if take < content_len { oversized = true; }
        }
        input.consume(consumed);
        if newline.is_some() { break; }
    }
    if oversized {
        return Err(semantic_error(ErrorCode::PayloadTooLarge, "command line exceeds configured limit"));
    }
    if bytes.last() == Some(&b'\r') { bytes.pop(); }
    String::from_utf8(bytes).map(Some).map_err(|_| semantic_error(ErrorCode::InvalidUtf8, "command line must be UTF-8"))
}

fn endpoint_fields(model: &ValidatorModel, key: EndpointKey) -> ValidatorResult<&EndpointSchemaSpec> {
    model.endpoint(key).ok_or_else(|| semantic_error(ErrorCode::MissingRequiredField, "selected endpoint is missing or ambiguous"))
}

fn merge_parameters(
    preset: Option<&Preset>, explicit: &[(String, String)],
) -> ValidatorResult<Vec<ParameterInput>> {
    let mut values = BTreeMap::<String, String>::new();
    if let Some(preset) = preset {
        values.extend(preset.parameters.iter().map(|(name, value)| (name.clone(), value.clone())));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in explicit {
        if !seen.insert(name.as_str()) {
            return Err(semantic_error(ErrorCode::DuplicateObjectKey, "duplicate explicit parameter name"));
        }
        values.insert(name.clone(), value.clone());
    }
    Ok(values.into_iter().map(|(name, text)| ParameterInput { name, text }).collect())
}

fn missing_required(endpoint: &EndpointSchemaSpec, inputs: &[ParameterInput]) -> Vec<String> {
    endpoint.params.iter()
        .filter(|field| !matches!(&field.ty, EndpointType::Optional(_)))
        .filter(|field| !inputs.iter().any(|input| input.name == field.name))
        .map(|field| field.name.clone()).collect()
}

fn validate_supplied(model: &ValidatorModel, endpoint: &EndpointSchemaSpec, inputs: &[ParameterInput], limits: Limits) -> ValidatorResult<()> {
    let mut seen = std::collections::BTreeSet::new();
    for input in inputs {
        if !seen.insert(input.name.as_str()) {
            return Err(semantic_error(ErrorCode::DuplicateObjectKey, "duplicate parameter name"));
        }
        let field = endpoint.params.iter().find(|field| field.name == input.name)
            .ok_or_else(|| semantic_error(ErrorCode::UnknownObjectKey, "unknown endpoint parameter"))?;
        params::validate_input(model, field, &input.text, limits)?;
    }
    Ok(())
}

fn resolve_selection<'a>(model: &'a ValidatorModel, options: &Options) -> ValidatorResult<(Option<EndpointKey>, Option<&'a Preset>)> {
    match (&options.preset, &options.service, &options.endpoint) {
        (Some(name), None, None) => {
            let preset = load::select_preset(model, name)?;
            Ok((Some(preset.endpoint), Some(preset)))
        }
        (None, Some(service), Some(endpoint)) => Ok((Some(load::select_endpoint(model, service, endpoint)?), None)),
        (None, None, None) => Ok((None, None)),
        _ => Err(ValidatorError::Usage("selection options are inconsistent".into())),
    }
}

fn limits_for(_: &Options) -> Limits {
    Limits::strict_default()
}

fn send_selected(
    model: &ValidatorModel,
    key: EndpointKey,
    values: &[ParameterInput],
    limits: Limits,
    bound: &mut dyn SessionReactor,
    session: &mut Option<Session>,
    output: &mut dyn Write,
) -> ValidatorResult<()> {
    let prepared = params::prepare_params(model, key, values, limits)?;
    if session.is_none() { *session = Some(session::new_session(SessionConfig::default())?); }
    let Some(current) = session.as_mut() else {
        return Err(semantic_error(ErrorCode::InternalError, "session initialization failed"));
    };
    if current.phase() == SessionPhase::Disconnected {
        session::connect(current, bound)?;
    }
    let sequence = session::submit(current, model, prepared, bound, limits)?;
    write_line(output, &format!("sent request {sequence}"))
}

/// Validates the model and runs the selected view using only explicit I/O and reactor handles.
pub fn run(
    model: &ValidatorModel,
    options: &Options,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    mut reactor: Option<&mut dyn SessionReactor>,
) -> ValidatorResult<()> {
    let limits = limits_for(options);
    load::validate_model(model, limits)?;
    if options.parameters.iter().any(|(name, _)| name.is_empty()) {
        return Err(semantic_error(ErrorCode::MissingRequiredField, "parameter name must not be empty"));
    }
    let (mut selected, preset) = resolve_selection(model, options)?;
    let mut values = merge_parameters(preset, &options.parameters)?;
    if let Some(key) = selected {
        let endpoint = endpoint_fields(model, key)?;
        validate_supplied(model, endpoint, &values, limits)?;
    } else if !values.is_empty() {
        return Err(semantic_error(ErrorCode::MissingRequiredField, "parameters require a selected endpoint"));
    }

    if options.view == ViewMode::List {
        render_list(model, output)?;
        if let Some(key) = selected {
            let endpoint = endpoint_fields(model, key)?;
            let missing = missing_required(endpoint, &values);
            if !missing.is_empty() {
                write_line(output, &format!("missing required parameters: {}", missing.iter().map(|s| escaped(s, limits.max_string_bytes as usize)).collect::<Vec<_>>().join(", ")))?;
            } else if !values.is_empty() {
                params::prepare_params(model, key, &values, limits)?;
            }
        }
        return Ok(());
    }

    let mut session: Option<Session> = None;
    loop {
        if options.view == ViewMode::Tui { write_line(output, "jmax-validator TUI")?; }
        write_line(output, "jmax-validator commands: list, select SERVICE ENDPOINT, set NAME=VALUE, validate, send, poll, close, quit")?;
        write_io(output, if options.view == ViewMode::Tui { &b"tui> "[..] } else { &b"> "[..] })?;
        output.flush().map_err(|source| ValidatorError::Io {
            operation: "write stdout", path: PathBuf::from("<stdout>"), source,
        })?;
        let line = match read_bounded_line(input, (limits.max_payload_bytes as usize).min(limits.max_string_bytes as usize).max(1)) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => { report_error(output, &error, limits)?; continue; }
        };
        let command_line = line.trim_start();
        if command_line.trim().is_empty() { continue; }
        let (command, argument) = command_line.split_once(char::is_whitespace).map_or((command_line, ""), |(cmd, rest)| (cmd, rest.trim_start()));
        let action: ValidatorResult<bool> = match command {
            "quit" => Ok(false),
            "list" => render_list(model, output).map(|()| true),
            "select" => {
                let mut parts = argument.split_whitespace();
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(service), Some(endpoint), None) => load::select_endpoint(model, service, endpoint).and_then(|key| {
                        selected = Some(key);
                        values.clear();
                        write_line(output, "selected endpoint")
                    }).map(|()| true),
                    _ => Err(ValidatorError::Usage("select requires SERVICE ENDPOINT".into())),
                }
            }
            "set" => match argument.split_once('=') {
                Some((name, text)) if !name.is_empty() => {
                    if text.len() > limits.max_string_bytes as usize {
                        Err(semantic_error(ErrorCode::PayloadTooLarge, "parameter value exceeds configured limit"))
                    } else if let Some(key) = selected {
                        endpoint_fields(model, key).and_then(|endpoint| match endpoint.params.iter().find(|field| field.name == name) {
                            None => Err(semantic_error(ErrorCode::UnknownObjectKey, "unknown endpoint parameter")),
                            Some(field) => params::validate_input(model, field, text, limits).and_then(|()| {
                                if let Some(existing) = values.iter_mut().find(|input| input.name == name) { existing.text = text.to_owned(); }
                                else { values.push(ParameterInput { name: name.to_owned(), text: text.to_owned() }); }
                                write_line(output, "parameter updated")
                            }).map(|()| true),
                        })
                    } else { Err(semantic_error(ErrorCode::MissingRequiredField, "select an endpoint before setting parameters")) }
                }
                _ => Err(ValidatorError::Usage("set requires NAME=VALUE".into())),
            },
            "validate" => match selected {
                None => Err(semantic_error(ErrorCode::MissingRequiredField, "select an endpoint before validation")),
                Some(key) => params::prepare_params(model, key, &values, limits).and_then(|_| write_line(output, "parameters valid")).map(|()| true),
            },
            "send" => match selected {
                None => Err(semantic_error(ErrorCode::MissingRequiredField, "select an endpoint before sending")),
                Some(key) => match reactor.as_deref_mut() {
                    None => Err(ValidatorError::State { phase: session.as_ref().map_or(SessionPhase::Disconnected, Session::phase), detail: "no reactor bound" }),
                    Some(bound) => send_selected(model, key, &values, limits, bound, &mut session, output).map(|()| true),
                },
            },
            "poll" => match reactor.as_deref_mut() {
                None => Err(ValidatorError::State { phase: session.as_ref().map_or(SessionPhase::Disconnected, Session::phase), detail: "no reactor bound" }),
                Some(bound) => match session.as_mut() {
                    None => Err(ValidatorError::State { phase: SessionPhase::Disconnected, detail: "session has not been started" }),
                    Some(current) => session::poll(current, bound).and_then(|event| {
                        match event {
                            None => write_line(output, "no event"),
                            Some(SessionEvent::Connected) => write_line(output, "connected"),
                            Some(SessionEvent::Closed) => write_line(output, "closed"),
                            Some(SessionEvent::Immediate { sequence, payload }) => write_line(output, &format!("response {sequence}: {}", escaped(std::str::from_utf8(payload.as_slice()).unwrap_or("<invalid UTF-8>"), limits.max_payload_bytes as usize))),
                            Some(SessionEvent::Stream { sequence, payload }) => write_line(output, &format!("stream {sequence}: {}", escaped(std::str::from_utf8(payload.as_slice()).unwrap_or("<invalid UTF-8>"), limits.max_payload_bytes as usize))),
                            Some(SessionEvent::RemoteError { sequence, payload }) => write_line(output, &format!("remote error {sequence}: {}", escaped(std::str::from_utf8(payload.as_slice()).unwrap_or("<invalid UTF-8>"), limits.max_payload_bytes as usize))),
                            Some(SessionEvent::StreamClosed { sequence }) => write_line(output, &format!("stream closed {sequence}")),
                        }
                    }).map(|()| true),
                },
            },
            "close" => match reactor.as_deref_mut() {
                None => Err(ValidatorError::State { phase: session.as_ref().map_or(SessionPhase::Disconnected, Session::phase), detail: "no reactor bound" }),
                Some(bound) => match session.as_mut() {
                    Some(current) => session::close(current, bound).and_then(|()| write_line(output, "close requested")).map(|()| true),
                    None => Err(ValidatorError::State { phase: SessionPhase::Disconnected, detail: "session has not been started" }),
                },
            },
            _ => Err(ValidatorError::Usage("unknown command".into())),
        };
        match action {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => report_error(output, &error, limits)?,
        }
    }

    if let (Some(current), Some(bound)) = (session.as_mut(), reactor.as_deref_mut()) {
        if !matches!(current.phase(), SessionPhase::Disconnected | SessionPhase::Closed) {
            session::close(current, bound)?;
        }
    }
    Ok(())
}
