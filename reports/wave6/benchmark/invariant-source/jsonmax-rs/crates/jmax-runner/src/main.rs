//! Deterministic offline runner. Live transports implement the library Transport trait.
use std::{collections::{BTreeMap, VecDeque}, env, fs::File, io::{self, Read, Write}, process::ExitCode};
use jmax_runner::{drive, scenario, CallRequest, Payload, Reply, Result, RunnerError, Step, Transport, MAX_SCENARIO_BYTES};

#[global_allocator]
static ALLOCATOR: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

const USAGE: &str = "Usage:\n  jmax-runner validate SCENARIO\n  jmax-runner run SCENARIO --replies JSON_LINES\n  jmax-runner --help\n  jmax-runner --version\n\nReplay consumes one complete JSON value per nonempty line, in call/receive order.\nScenario files use the version 1 format documented in contracts/jmax-runner.md.\n";

fn read_bounded(path: &str) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|_| RunnerError::Io {
        operation: "open input", detail: "unable to open input file".into(),
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_SCENARIO_BYTES as u64 + 1).read_to_end(&mut bytes)
        .map_err(|_| RunnerError::Io { operation: "read input", detail: "unable to read input file".into() })?;
    if bytes.len() > MAX_SCENARIO_BYTES {
        return Err(RunnerError::InvalidScenario { step: None, detail: "input exceeds byte limit".into() });
    }
    Ok(bytes)
}

struct Replay {
    connections: BTreeMap<String, ()>,
    replies: VecDeque<Payload>,
}
impl Replay {
    fn next_reply(&mut self, sequence: Option<u64>) -> Result<Reply> {
        let body = self.replies.pop_front().ok_or_else(|| RunnerError::Transport {
            connection: "replay".into(), detail: "reply input exhausted".into(),
        })?;
        Ok(Reply { sequence, error_code: None, body })
    }
    fn require_connection(&self, name: &str) -> Result<()> {
        if self.connections.contains_key(name) { Ok(()) }
        else { Err(RunnerError::UnknownConnection(name.into())) }
    }
}
impl Transport for Replay {
    fn connect(&mut self, name: &str, _uri: &str, _role: &str, _timeout_ms: u32) -> Result<()> {
        if self.connections.contains_key(name) {
            return Err(RunnerError::Transport { connection: name.into(), detail: "connection already open".into() });
        }
        self.connections.insert(name.into(), ());
        Ok(())
    }
    fn call(&mut self, name: &str, request: &CallRequest, _timeout_ms: u32) -> Result<Reply> {
        self.require_connection(name)?;
        self.next_reply(Some(request.sequence))
    }
    fn receive(&mut self, name: &str, _timeout_ms: u32) -> Result<Reply> {
        self.require_connection(name)?;
        self.next_reply(None)
    }
    fn close(&mut self, name: &str) -> Result<()> {
        self.connections.remove(name).map(|_| ()).ok_or_else(|| RunnerError::UnknownConnection(name.into()))
    }
}

fn execute(args: &[String], output: &mut dyn Write) -> Result<()> {
    let write_error = |_| RunnerError::Io { operation: "write output", detail: "unable to write output".into() };
    match args {
        [] => Err(RunnerError::InvalidScenario { step: None, detail: USAGE.into() }),
        [flag] if flag == "--help" || flag == "-h" => output.write_all(USAGE.as_bytes()).map_err(write_error),
        [flag] if flag == "--version" => writeln!(output, "jmax-runner {}", env!("CARGO_PKG_VERSION")).map_err(write_error),
        [command, path] if command == "validate" => {
            let model = scenario::parse_scenario(&read_bounded(path)?)?;
            scenario::validate_scenario(&model)?;
            writeln!(output, "valid: {} steps", model.steps.len()).map_err(write_error)
        }
        [command, path, option, replies_path] if command == "run" && option == "--replies" => {
            let model = scenario::parse_scenario(&read_bounded(path)?)?;
            scenario::validate_scenario(&model)?;
            let bytes = read_bounded(replies_path)?;
            let expected_replies = model.steps.iter().filter(|step| matches!(step, Step::Call { .. } | Step::Receive { .. })).count();
            let mut replies = VecDeque::new();
            for line in bytes.split(|byte| *byte == b'\n') {
                let start = line.iter().position(|byte| !matches!(*byte, b' ' | b'\t' | b'\r'));
                if let Some(start) = start {
                    if replies.len() >= expected_replies {
                        return Err(RunnerError::InvalidScenario { step: None, detail: "unused replay replies".into() });
                    }
                    let end = line.iter().rposition(|byte| !matches!(*byte, b' ' | b'\t' | b'\r')).map_or(start, |index| index + 1);
                    let json = &line[start..end];
                    drive::validate_reply(json)?;
                    replies.push_back(Payload::from_slice(json));
                }
            }
            let mut replay = Replay { connections: BTreeMap::new(), replies };
            let report = drive::run_scenario(&model, &mut replay)?;
            if !replay.replies.is_empty() {
                return Err(RunnerError::InvalidScenario { step: None, detail: "unused replay replies".into() });
            }
            writeln!(output, "passed: {} steps, {} assertions, {} role/endpoint pairs",
                report.steps_completed, report.assertions_passed, report.calls.len()).map_err(write_error)
        }
        _ => Err(RunnerError::InvalidScenario { step: None, detail: USAGE.into() }),
    }
}

fn main() -> ExitCode {
    let mut args = Vec::new();
    for arg in env::args_os().skip(1) {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(_) => {
                let _ = writeln!(io::stderr().lock(), "jmax-runner: arguments must be valid UTF-8");
                return ExitCode::from(2);
            }
        }
    }
    match execute(&args, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "jmax-runner: {error}");
            ExitCode::from(match error { RunnerError::InvalidScenario { .. } => 2, _ => 1 })
        }
    }
}
