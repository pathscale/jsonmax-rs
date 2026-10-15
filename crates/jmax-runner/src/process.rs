//! Validated boot configuration, literal redaction, and owned child lifecycles.
//!
//! Polling performs no scheduling. The caller supplies a prompt readiness probe
//! and schedules subsequent polls; termination and destruction reap the child.

use super::*;

#[cfg(feature = "std")]
use std::{
    io,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

/// Validate boot fields without consulting or changing the host environment.
/// Diagnostics identify constraints without echoing any supplied values.
pub fn validate_boot(spec: &BootSpec) -> Result<()> {
    if spec.program.is_empty() {
        return Err(process_error("program must not be empty"));
    }
    if spec.program.contains('\0') {
        return Err(process_error("program must not contain NUL"));
    }
    if spec.startup_timeout_ms == 0 {
        return Err(process_error("startup_timeout_ms must be positive"));
    }
    if spec.args.iter().any(|argument| argument.contains('\0')) {
        return Err(process_error("arguments must not contain NUL"));
    }
    if let Some(cwd) = &spec.cwd {
        if cwd.is_empty() {
            return Err(process_error("explicit cwd must not be empty"));
        }
        if cwd.contains('\0') {
            return Err(process_error("cwd must not contain NUL"));
        }
    }
    for (key, value) in &spec.env {
        if key.is_empty() {
            return Err(process_error("environment keys must not be empty"));
        }
        if key.contains('=') {
            return Err(process_error("environment keys must not contain '='"));
        }
        if key.contains('\0') {
            return Err(process_error("environment keys must not contain NUL"));
        }
        if value.contains('\0') {
            return Err(process_error("environment values must not contain NUL"));
        }
    }
    Ok(())
}

/// Apply exactly the root policy's literal, Unicode-safe redaction semantics.
pub fn redact(input: &str, policy: &RedactionPolicy) -> String {
    policy.redact(input)
}

fn allocation_error() -> RunnerError {
    Error::new(ErrorCode::BudgetExhausted, ErrorSource::Decode, 0,
        "process diagnostic allocation failed").into()
}

fn owned_string(value: &str) -> Result<String> {
    let mut owned = String::new();
    reserve_diagnostic(&mut owned, value.len())?;
    owned.push_str(value);
    Ok(owned)
}

fn reserve_diagnostic(value: &mut String, additional: usize) -> Result<()> {
    value.try_reserve_exact(additional).map_err(|_| allocation_error())
}

fn process_error(detail: &'static str) -> RunnerError {
    match owned_string(detail) {
        Ok(detail) => RunnerError::Process { detail },
        Err(error) => error,
    }
}

/// A readiness attempt must return promptly and own any I/O deadline it needs.
/// Errors are propagated unchanged, so probes must sanitize their diagnostics.
#[cfg(feature = "std")]
pub trait ReadinessProbe {
    fn is_ready(&mut self) -> Result<bool>;
}

/// The unique owner of a child, its startup deadline, and its cached state.
///
/// The spawned PID remains available after exit. No child streams are inherited
/// or piped. Dropping the owner attempts to kill and reap any remaining child.
#[cfg(feature = "std")]
pub struct ManagedProcess {
    child: Child,
    pid: u32,
    deadline: Instant,
    state: ProcessState,
    policy: RedactionPolicy,
}

#[cfg(feature = "std")]
impl ManagedProcess {
    /// Invoke the specified executable directly with isolated standard streams.
    pub fn spawn(spec: &BootSpec, policy: RedactionPolicy) -> Result<Self> {
        validate_boot(spec)?;
        // Check the deadline before creating a child. After a successful spawn,
        // construction is infallible and immediately transfers cleanup ownership.
        let deadline = startup_deadline(Instant::now(),
            Duration::from_millis(u64::from(spec.startup_timeout_ms)))?;
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        if spec.clear_env { command.env_clear(); }
        command.envs(&spec.env);
        if let Some(cwd) = &spec.cwd { command.current_dir(cwd); }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let child = command.spawn()
            .map_err(|error| io_failure(&policy, "process spawn failed", &error))?;
        let pid = child.id();
        Ok(Self { child, pid, deadline, state: ProcessState::Starting, policy })
    }

    /// Return the original PID, including after termination or natural exit.
    pub fn id(&self) -> u32 { self.pid }

    /// Check liveness once and attempt readiness at most once while starting.
    ///
    /// An exit observed by the status check takes precedence over readiness and
    /// timeout. The deadline is checked on both sides of a successful probe, so
    /// a probe completing at or beyond the deadline cannot establish readiness.
    pub fn poll(&mut self, probe: &mut dyn ReadinessProbe) -> Result<ProcessState> {
        self.poll_with_clock(probe, Instant::now)
    }

    // A private clock seam makes deadline boundaries deterministic in tests.
    fn poll_with_clock(
        &mut self,
        probe: &mut dyn ReadinessProbe,
        mut now: impl FnMut() -> Instant,
    ) -> Result<ProcessState> {
        if matches!(self.state, ProcessState::Exited { .. }) { return Ok(self.state); }
        if let Some(status) = self.child.try_wait()
            .map_err(|error| io_failure(&self.policy, "process status check failed", &error))? {
            return Ok(self.record_exit(status));
        }
        if self.state == ProcessState::Ready { return Ok(self.state); }
        if now() >= self.deadline { return self.startup_timeout(); }
        let ready = match probe.is_ready() {
            Ok(ready) => ready,
            Err(error) => {
                // Cleanup errors cannot replace the probe's original error.
                // If cleanup fails, self still owns the child for a retry/Drop.
                let _ = self.terminate();
                return Err(error);
            }
        };
        if now() >= self.deadline { return self.startup_timeout(); }
        if ready { self.state = ProcessState::Ready; }
        Ok(self.state)
    }

    /// Kill and reap a live child, preserving a natural exit when observed.
    /// A cleanup error leaves ownership and the last confirmed state intact.
    pub fn terminate(&mut self) -> Result<ProcessState> {
        if matches!(self.state, ProcessState::Exited { .. }) { return Ok(self.state); }
        let status = stop_child(&mut self.child, &self.policy)?;
        Ok(self.record_exit(status))
    }

    fn record_exit(&mut self, status: ExitStatus) -> ProcessState {
        self.state = ProcessState::Exited { code: status.code() };
        self.state
    }

    fn startup_timeout(&mut self) -> Result<ProcessState> {
        let _ = self.terminate();
        Err(match owned_string("process startup") {
            Ok(operation) => RunnerError::Timeout { operation },
            Err(error) => error,
        })
    }
}

#[cfg(feature = "std")]
impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !matches!(self.state, ProcessState::Exited { .. }) {
            // This path does not allocate diagnostics, invoke probes, or panic
            // on OS failures. In particular it also works during unwinding.
            reap_best_effort(&mut self.child);
        }
    }
}

#[cfg(feature = "std")]
fn startup_deadline(now: Instant, timeout: Duration) -> Result<Instant> {
    now.checked_add(timeout)
        .ok_or_else(|| process_error("startup deadline exceeds monotonic clock range"))
}

// OS error Display implementations can contain paths or custom body contents.
// Select a fixed error classification instead, then apply the caller's policy
// to the whole diagnostic. No arguments, environment, or probe data are read.
#[cfg(feature = "std")]
fn io_failure(policy: &RedactionPolicy, operation: &'static str, error: &io::Error) -> RunnerError {
    let reason = match error.kind() {
        io::ErrorKind::NotFound => "not found",
        io::ErrorKind::PermissionDenied => "permission denied",
        io::ErrorKind::InvalidInput => "invalid input",
        io::ErrorKind::Interrupted => "interrupted",
        io::ErrorKind::WouldBlock => "temporarily unavailable",
        io::ErrorKind::TimedOut => "operating system timeout",
        io::ErrorKind::Unsupported => "unsupported operation",
        io::ErrorKind::OutOfMemory => "out of memory",
        _ => "operating system error",
    };
    let mut detail = String::new();
    let Some(length) = operation.len().checked_add(2).and_then(|n| n.checked_add(reason.len())) else {
        return allocation_error();
    };
    if let Err(error) = reserve_diagnostic(&mut detail, length) { return error; }
    detail.push_str(operation);
    detail.push_str(": ");
    detail.push_str(reason);
    RunnerError::Process { detail: redact(&detail, policy) }
}

// The real Child stays in ManagedProcess. This private interface permits exact
// fault vectors for OS cleanup races without creating processes with leaked PIDs
// or depending on host permissions and signal timing.
#[cfg(feature = "std")]
trait ChildLifecycle {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

#[cfg(feature = "std")]
impl ChildLifecycle for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> { Child::try_wait(self) }
    fn kill(&mut self) -> io::Result<()> { Child::kill(self) }
    fn wait(&mut self) -> io::Result<ExitStatus> { Child::wait(self) }
}

#[cfg(feature = "std")]
fn stop_child(child: &mut impl ChildLifecycle, policy: &RedactionPolicy) -> Result<ExitStatus> {
    if let Some(status) = child.try_wait()
        .map_err(|error| io_failure(policy, "process status check failed", &error))? {
        return Ok(status);
    }
    if let Err(error) = child.kill() {
        // The child may have exited between try_wait and kill. Recheck before
        // reporting the kill failure; a secondary status error must not hide it.
        if let Ok(Some(status)) = child.try_wait() { return Ok(status); }
        return Err(io_failure(policy, "process termination failed", &error));
    }
    child.wait().map_err(|error| io_failure(policy, "process reap failed", &error))
}

#[cfg(feature = "std")]
fn reap_best_effort(child: &mut impl ChildLifecycle) {
    if let Ok(Some(_)) = child.try_wait() { return; }
    if child.kill().is_ok() {
        let _ = child.wait();
    } else {
        // Reap an exit race, but do not wait indefinitely after a failed kill
        // when there is no confirmation that the child has stopped.
        let _ = child.try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec};

    fn boot() -> BootSpec {
        BootSpec {
            program: "program".into(), args: Vec::new(), env: BTreeMap::new(),
            cwd: None, clear_env: false, startup_timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    fn policy(secrets: &[&str]) -> RedactionPolicy {
        RedactionPolicy::new(secrets.iter().map(|secret| (*secret).into()).collect())
    }

    fn detail(error: RunnerError) -> String {
        match error {
            RunnerError::Process { detail } => detail,
            other => panic!("expected process error, got {other:?}"),
        }
    }

    #[test]
    fn boot_accepts_literal_arguments_unicode_and_timeout_boundaries_without_mutation() {
        let mut spec = boot();
        spec.program = " executable λ ".into();
        spec.args = vec!["".into(), " ".into(), "$(literal); | & > < * ? ' \"".into(), "🚀\n".into()];
        spec.env.insert(" key λ ".into(), "".into());
        spec.env.insert("TOKEN".into(), "a=b\nsecret 🚀".into());
        spec.cwd = Some(" ".into());
        for timeout in [1, DEFAULT_TIMEOUT_MS, u32::MAX] {
            for clear_env in [false, true] {
                spec.startup_timeout_ms = timeout;
                spec.clear_env = clear_env;
                let before = spec.clone();
                validate_boot(&spec).unwrap();
                assert_eq!(spec, before);
            }
        }
        validate_boot(&boot()).unwrap();
    }

    #[test]
    fn boot_rejects_every_invalid_field_with_fixed_private_diagnostics() {
        let mut cases = Vec::new();
        let mut spec = boot();
        spec.program.clear();
        cases.push((spec, "program must not be empty"));
        let mut spec = boot();
        spec.startup_timeout_ms = 0;
        cases.push((spec, "startup_timeout_ms must be positive"));
        for value in ["\0", "private-value\0suffix", "λ\0🚀"] {
            let mut spec = boot();
            spec.program = value.into();
            cases.push((spec, "program must not contain NUL"));
            for index in 0..3 {
                let mut spec = boot();
                spec.args = vec!["safe".into(); 3];
                spec.args[index] = value.into();
                cases.push((spec, "arguments must not contain NUL"));
            }
            let mut spec = boot();
            spec.cwd = Some(value.into());
            cases.push((spec, "cwd must not contain NUL"));
            let mut spec = boot();
            spec.env.insert(value.into(), "private-value".into());
            cases.push((spec, "environment keys must not contain NUL"));
            let mut spec = boot();
            spec.env.insert("TOKEN".into(), value.into());
            cases.push((spec, "environment values must not contain NUL"));
        }
        let mut spec = boot();
        spec.cwd = Some(String::new());
        cases.push((spec, "explicit cwd must not be empty"));
        let mut spec = boot();
        spec.env.insert(String::new(), "private-value".into());
        cases.push((spec, "environment keys must not be empty"));
        for key in ["=", "=TOKEN", "TOKEN=", "private=value", "λ=🚀"] {
            let mut spec = boot();
            spec.env.insert(key.into(), "private-value".into());
            cases.push((spec, "environment keys must not contain '='"));
        }
        for (spec, expected) in cases {
            let before = spec.clone();
            let error = validate_boot(&spec).unwrap_err();
            assert!(!error.to_string().contains("private-value"));
            assert_eq!(detail(error), expected);
            assert_eq!(spec, before);
        }
    }

    #[test]
    fn redaction_vectors_cover_longest_original_boundary_literal_and_unicode_matching() {
        let vectors: &[(&[&str], &str, &str)] = &[
            (&[], "", ""),
            (&["", ""], "λ🚀\0\n", "λ🚀\0\n"),
            (&["secret"], "secret secretsecret", "[REDACTED] [REDACTED][REDACTED]"),
            (&["secret"], "Secret SECRET secret", "Secret SECRET [REDACTED]"),
            (&["a", "ab", "abc", "ab"], "abcaba", "[REDACTED][REDACTED][REDACTED]"),
            (&["ab", "bc"], "abc", "[REDACTED]c"),
            (&["bc", "abc"], "zabc", "z[REDACTED]"),
            (&["aba", "ba"], "ababa", "[REDACTED][REDACTED]"),
            (&[".*", "$1", "\\"], "x.*$1\\y", "x[REDACTED][REDACTED][REDACTED]y"),
            (&["é"], "é e\u{301} É", "[REDACTED] e\u{301} É"),
            (&["λ", "λ🚀", "🚀"], "aλ🚀λ🚀b", "a[REDACTED][REDACTED]b"),
            (&["\u{301}"], "e\u{301}", "e[REDACTED]"),
            (&["\0"], "a\0b", "a[REDACTED]b"),
            (&["a", "[REDACTED]"], "a[REDACTED]a", "[REDACTED][REDACTED][REDACTED]"),
            (&["REDACTED"], "[REDACTED]", "[[REDACTED]]"),
        ];
        for &(secrets, input, expected) in vectors {
            let policy = policy(secrets);
            assert_eq!(redact(input, &policy), expected);
            assert_eq!(redact(input, &policy), policy.redact(input));
            assert_eq!(redact(input, &policy).as_bytes(), expected.as_bytes());
        }
    }

    #[test]
    fn secret_order_duplicates_and_empty_entries_do_not_change_redaction() {
        let input = "aaaa abc λ🚀 λ abc";
        let secrets = ["", "a", "aa", "abc", "λ", "λ🚀", "aa", ""];
        let expected = "[REDACTED][REDACTED] [REDACTED] [REDACTED] [REDACTED] [REDACTED]";
        for offset in 0..secrets.len() {
            let mut ordered = secrets.to_vec();
            ordered.rotate_left(offset);
            assert_eq!(redact(input, &policy(&ordered)), expected);
            ordered.reverse();
            assert_eq!(redact(input, &policy(&ordered)), expected);
        }
        let policy = policy(&["a", "[REDACTED]"]);
        assert_eq!(redact("a", &policy), REDACTED);
        assert_eq!(redact("unmatched λ🚀", &RedactionPolicy::default()), "unmatched λ🚀");
    }

    #[test]
    fn owned_diagnostics_report_fallible_capacity_failure_without_panicking() {
        let mut text = String::from("preserved diagnostic");
        let error = reserve_diagnostic(&mut text, usize::MAX).unwrap_err();
        assert_eq!(error, RunnerError::Json(Error::new(
            ErrorCode::BudgetExhausted, ErrorSource::Decode, 0, "process diagnostic allocation failed")));
        assert_eq!(text, "preserved diagnostic");
        reserve_diagnostic(&mut text, 0).unwrap();
        assert_eq!(owned_string("").unwrap(), "");
        assert_eq!(owned_string("λ🚀").unwrap(), "λ🚀");
        assert_eq!(detail(process_error("fixed diagnostic")), "fixed diagnostic");
    }

    #[cfg(feature = "std")]
    mod lifecycle {
        use super::*;
        use alloc::{collections::VecDeque, format};
        use std::io::{Read, Write};

        const FIXTURE_MODE: &str = "JMAX_RUNNER_PROCESS_FIXTURE_MODE";
        const FIXTURE_TEST: &str = "process::tests::lifecycle::child_fixture";
        const ARGUMENT: &str = "literal $(exit 89); & | > < * ? λ🚀";

        fn fixture_spec(mode: &str) -> BootSpec {
            let mut spec = boot();
            spec.program = std::env::current_exe().unwrap().into_os_string().into_string().unwrap();
            spec.args = ["--exact", FIXTURE_TEST, "--ignored", "--nocapture", "--test-threads=1"]
                .iter().map(|value| (*value).into()).collect();
            spec.env.insert(FIXTURE_MODE.into(), mode.into());
            spec.clear_env = true;
            spec.startup_timeout_ms = u32::MAX;
            spec
        }

        fn child(mode: &str) -> ManagedProcess {
            ManagedProcess::spawn(&fixture_spec(mode), RedactionPolicy::default()).unwrap()
        }

        // A portable real executable fixture. The parent selects only this test
        // in a fresh copy of its test executable; ordinary suites skip it.
        #[test]
        #[ignore = "internal subprocess fixture selected by lifecycle tests"]
        fn child_fixture() {
            match std::env::var(FIXTURE_MODE).as_deref() {
                Ok("hold") => {
                    let lock = std::sync::Mutex::new(());
                    let condition = std::sync::Condvar::new();
                    let _guard = condition.wait_while(lock.lock().unwrap(), |_| true).unwrap();
                    panic!("held child was unexpectedly released");
                }
                Ok("exit-7") => std::process::exit(7),
                Ok("inspect") => {
                    assert_eq!(std::env::var("JMAX_LITERAL_VALUE").unwrap(), " value=λ🚀\n ");
                    assert!(std::env::args().any(|argument| argument == ARGUMENT));
                    let expected = std::env::var("JMAX_EXPECTED_CWD").unwrap();
                    assert_eq!(std::env::current_dir().unwrap(), std::path::PathBuf::from(expected));
                    if let Ok(key) = std::env::var("JMAX_INHERITED_KEY") {
                        match std::env::var("JMAX_EXPECTED_INHERITED") {
                            Ok(expected) => assert_eq!(std::env::var(key).unwrap(), expected),
                            Err(_) => assert!(std::env::var_os(key).is_none()),
                        }
                    }
                    let mut byte = [0];
                    assert_eq!(std::io::stdin().read(&mut byte).unwrap(), 0);
                    // Exceed ordinary pipe capacities. Null streams cannot fill.
                    let block = [b'x'; 16 * 1024];
                    for _ in 0..16 {
                        std::io::stdout().write_all(&block).unwrap();
                        std::io::stderr().write_all(&block).unwrap();
                    }
                    std::process::exit(0);
                }
                other => panic!("invalid child fixture mode: {other:?}"),
            }
        }

        struct Probe { outcome: Result<bool>, calls: usize }

        impl Probe {
            fn returning(outcome: Result<bool>) -> Self { Self { outcome, calls: 0 } }
        }

        impl ReadinessProbe for Probe {
            fn is_ready(&mut self) -> Result<bool> {
                self.calls += 1;
                self.outcome.clone()
            }
        }

        struct NoProbe;
        impl ReadinessProbe for NoProbe {
            fn is_ready(&mut self) -> Result<bool> { panic!("probe must not run") }
        }

        fn assert_reaped(process: &mut ManagedProcess) -> ProcessState {
            let state = process.state;
            assert!(matches!(state, ProcessState::Exited { .. }));
            assert_eq!(process.child.try_wait().unwrap().unwrap().code(), match state {
                ProcessState::Exited { code } => code,
                _ => panic!("expected cached exit"),
            });
            assert_eq!(process.terminate().unwrap(), state);
            assert_eq!(process.poll_with_clock(&mut NoProbe, || panic!("clock must not run")).unwrap(), state);
            state
        }

        #[test]
        fn readiness_has_one_probe_per_poll_then_only_liveness_checks() {
            let mut process = child("hold");
            let pid = process.id();
            assert_ne!(pid, 0);
            assert_eq!(process.state, ProcessState::Starting);
            assert!(process.child.stdin.is_none());
            assert!(process.child.stdout.is_none());
            assert!(process.child.stderr.is_none());
            let mut probe = Probe::returning(Ok(false));
            assert_eq!(process.poll(&mut probe).unwrap(), ProcessState::Starting);
            assert_eq!(probe.calls, 1);
            probe.outcome = Ok(true);
            assert_eq!(process.poll(&mut probe).unwrap(), ProcessState::Ready);
            assert_eq!(probe.calls, 2);
            process.deadline = Instant::now();
            assert_eq!(process.poll_with_clock(&mut NoProbe, || panic!("ready ignores startup deadline")).unwrap(), ProcessState::Ready);
            process.terminate().unwrap();
            assert_reaped(&mut process);
            assert_eq!(process.id(), pid);
        }

        #[test]
        fn observed_natural_exit_wins_over_expired_deadline_and_ready_probe() {
            let mut process = child("exit-7");
            let pid = process.id();
            assert_eq!(process.child.wait().unwrap().code(), Some(7));
            process.deadline = Instant::now();
            let mut probe = Probe::returning(Ok(true));
            assert_eq!(process.poll_with_clock(&mut probe, || panic!("exit ignores deadline")).unwrap(),
                ProcessState::Exited { code: Some(7) });
            assert_eq!(probe.calls, 0);
            assert_eq!(assert_reaped(&mut process), ProcessState::Exited { code: Some(7) });
            assert_eq!(process.id(), pid);
        }

        #[test]
        fn ready_process_detects_later_exit_without_probing() {
            let mut process = child("hold");
            process.poll(&mut Probe::returning(Ok(true))).unwrap();
            process.child.kill().unwrap();
            let expected = process.child.wait().unwrap().code();
            assert_eq!(process.poll(&mut NoProbe).unwrap(), ProcessState::Exited { code: expected });
            assert_reaped(&mut process);
        }

        #[test]
        fn termination_is_idempotent_for_starting_ready_and_naturally_exited_children() {
            for ready in [false, true] {
                let mut process = child("hold");
                if ready { process.poll(&mut Probe::returning(Ok(true))).unwrap(); }
                let pid = process.id();
                let state = process.terminate().unwrap();
                assert_eq!(assert_reaped(&mut process), state);
                assert_eq!(process.id(), pid);
            }
            let mut process = child("exit-7");
            process.child.wait().unwrap();
            assert_eq!(process.terminate().unwrap(), ProcessState::Exited { code: Some(7) });
            assert_reaped(&mut process);
        }

        #[test]
        fn startup_deadline_vectors_use_checked_monotonic_arithmetic() {
            let now = Instant::now();
            for timeout in [Duration::ZERO, Duration::from_millis(1),
                Duration::from_millis(u64::from(u32::MAX)), Duration::MAX] {
                match now.checked_add(timeout) {
                    Some(expected) => assert_eq!(startup_deadline(now, timeout).unwrap(), expected),
                    None => assert_eq!(detail(startup_deadline(now, timeout).unwrap_err()),
                        "startup deadline exceeds monotonic clock range"),
                }
            }
        }

        #[test]
        fn deadline_at_or_before_poll_times_out_without_a_probe_and_reaps() {
            for delta in [Duration::ZERO, Duration::from_millis(1)] {
                let mut process = child("hold");
                let deadline = process.deadline;
                let mut probe = Probe::returning(Ok(true));
                let error = process.poll_with_clock(&mut probe, || deadline + delta).unwrap_err();
                assert_eq!(error, RunnerError::Timeout { operation: "process startup".into() });
                assert_eq!(probe.calls, 0);
                assert_reaped(&mut process);
            }
        }

        #[test]
        fn probe_completion_at_deadline_or_later_times_out_even_when_ready() {
            for ready in [false, true] {
                for delta in [Duration::ZERO, Duration::from_millis(1)] {
                    let mut process = child("hold");
                    let deadline = process.deadline;
                    let mut clock = [deadline - Duration::from_millis(1), deadline + delta].into_iter();
                    let mut probe = Probe::returning(Ok(ready));
                    assert!(matches!(process.poll_with_clock(&mut probe, || clock.next().unwrap()),
                        Err(RunnerError::Timeout { .. })));
                    assert_eq!(probe.calls, 1);
                    assert!(clock.next().is_none());
                    assert_reaped(&mut process);
                }
            }
        }

        #[test]
        fn readiness_immediately_before_deadline_is_accepted_and_pending_is_preserved() {
            for ready in [false, true] {
                let mut process = child("hold");
                let before = process.deadline - Duration::from_nanos(1);
                let mut clock_calls = 0;
                let mut probe = Probe::returning(Ok(ready));
                assert_eq!(process.poll_with_clock(&mut probe, || { clock_calls += 1; before }).unwrap(),
                    if ready { ProcessState::Ready } else { ProcessState::Starting });
                assert_eq!(clock_calls, 2);
                assert_eq!(probe.calls, 1);
                process.terminate().unwrap();
                assert_reaped(&mut process);
            }
        }

        #[test]
        fn all_probe_error_variants_are_preserved_and_reap_the_child() {
            let errors = [
                RunnerError::InvalidScenario { step: Some(3), detail: "invalid probe model".into() },
                RunnerError::UnknownConnection("probe".into()),
                RunnerError::UnknownResponse("probe".into()),
                RunnerError::Transport { connection: "probe".into(), detail: "safe failure".into() },
                RunnerError::Assertion { step: 4, detail: "probe assertion".into() },
                RunnerError::Timeout { operation: "probe".into() },
                RunnerError::Process { detail: "probe process".into() },
                RunnerError::Io { operation: "probe", detail: "safe I/O failure".into() },
                RunnerError::Json(Error::new(ErrorCode::InvalidToken, ErrorSource::Lexer, 2, "probe JSON")),
            ];
            for expected in errors {
                let mut process = child("hold");
                let pid = process.id();
                let mut probe = Probe::returning(Err(expected.clone()));
                let before = process.deadline - Duration::from_millis(1);
                let mut calls = 0;
                assert_eq!(process.poll_with_clock(&mut probe, || {
                    calls += 1;
                    assert_eq!(calls, 1, "probe failure must return without another clock check");
                    before
                }).unwrap_err(), expected);
                assert_eq!(probe.calls, 1);
                assert_reaped(&mut process);
                assert_eq!(process.id(), pid);
            }
        }

        #[test]
        fn spawn_applies_cwd_literal_arguments_environment_and_null_streams() {
            let inherited = ["PATH", "HOME", "USERPROFILE"].into_iter()
                .find_map(|key| std::env::var(key).ok().map(|value| (key, value)));
            let cwd = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let cwd = cwd.into_os_string().into_string().unwrap();
            for (clear_env, override_inherited) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut spec = fixture_spec("inspect");
                spec.clear_env = clear_env;
                spec.cwd = Some(cwd.clone());
                spec.args.extend(["--skip".into(), ARGUMENT.into()]);
                spec.env.insert("JMAX_LITERAL_VALUE".into(), " value=λ🚀\n ".into());
                spec.env.insert("JMAX_EXPECTED_CWD".into(), cwd.clone());
                if let Some((key, value)) = &inherited {
                    spec.env.insert("JMAX_INHERITED_KEY".into(), (*key).into());
                    if override_inherited {
                        spec.env.insert((*key).into(), "explicit replacement λ🚀".into());
                        spec.env.insert("JMAX_EXPECTED_INHERITED".into(), "explicit replacement λ🚀".into());
                    } else if !clear_env {
                        spec.env.insert("JMAX_EXPECTED_INHERITED".into(), value.clone());
                    }
                }
                let before = spec.clone();
                let mut process = ManagedProcess::spawn(&spec, RedactionPolicy::default()).unwrap();
                assert_eq!(process.child.wait().unwrap().code(), Some(0));
                assert_eq!(process.poll(&mut NoProbe).unwrap(), ProcessState::Exited { code: Some(0) });
                assert_reaped(&mut process);
                assert_eq!(spec, before);
            }
        }

        #[test]
        fn invalid_boot_is_rejected_before_spawning_and_spawn_failures_are_private() {
            let mut spec = fixture_spec("hold");
            spec.args.push("private-argument\0".into());
            let expected = validate_boot(&spec).unwrap_err();
            assert_eq!(ManagedProcess::spawn(&spec, RedactionPolicy::default()).err().unwrap(), expected);

            for invalid_cwd in [false, true] {
                let mut spec = fixture_spec("hold");
                spec.args.push("private-argument".into());
                spec.env.insert("TOKEN".into(), "private-environment".into());
                if invalid_cwd {
                    spec.cwd = Some(spec.program.clone()); // An executable file is not a directory.
                } else {
                    spec.program.push_str("/nonexistent-private-program");
                }
                let error = ManagedProcess::spawn(&spec, policy(&["process spawn failed"]))
                    .err().expect("invalid executable/cwd must fail");
                let diagnostic = detail(error);
                assert!(diagnostic.starts_with(REDACTED));
                for secret in ["private-argument", "private-environment", "nonexistent-private-program", spec.program.as_str()] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }

        #[test]
        fn io_error_details_are_fixed_then_policy_redacted_without_custom_bodies() {
            for (kind, expected) in [
                (io::ErrorKind::NotFound, "not found"),
                (io::ErrorKind::PermissionDenied, "permission denied"),
                (io::ErrorKind::InvalidInput, "invalid input"),
                (io::ErrorKind::Interrupted, "interrupted"),
                (io::ErrorKind::WouldBlock, "temporarily unavailable"),
                (io::ErrorKind::TimedOut, "operating system timeout"),
                (io::ErrorKind::Unsupported, "unsupported operation"),
                (io::ErrorKind::OutOfMemory, "out of memory"),
                (io::ErrorKind::Other, "operating system error"),
            ] {
                let error = io::Error::new(kind, "private-argument environment-value probe-body");
                let diagnostic = detail(io_failure(&RedactionPolicy::default(), "operation failed", &error));
                assert_eq!(diagnostic, format!("operation failed: {expected}"));
                assert!(!diagnostic.contains("private-argument"));
                assert_eq!(detail(io_failure(&policy(&["operation failed", expected]), "operation failed", &error)),
                    "[REDACTED]: [REDACTED]");
            }
        }

        struct ScriptedChild {
            events: Vec<&'static str>,
            checks: VecDeque<io::Result<Option<ExitStatus>>>,
            kills: VecDeque<io::Result<()>>,
            waits: VecDeque<io::Result<ExitStatus>>,
        }

        impl ScriptedChild {
            fn new(checks: Vec<io::Result<Option<ExitStatus>>>, kills: Vec<io::Result<()>>,
                waits: Vec<io::Result<ExitStatus>>) -> Self {
                Self { events: Vec::new(), checks: checks.into(), kills: kills.into(), waits: waits.into() }
            }

            fn assert_consumed(&self, expected: &[&str]) {
                assert_eq!(self.events, expected);
                assert!(self.checks.is_empty());
                assert!(self.kills.is_empty());
                assert!(self.waits.is_empty());
            }
        }

        impl ChildLifecycle for ScriptedChild {
            fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
                self.events.push("try_wait");
                self.checks.pop_front().expect("unexpected status check")
            }
            fn kill(&mut self) -> io::Result<()> {
                self.events.push("kill");
                self.kills.pop_front().expect("unexpected kill")
            }
            fn wait(&mut self) -> io::Result<ExitStatus> {
                self.events.push("wait");
                self.waits.pop_front().expect("unexpected reap")
            }
        }

        fn os_error(kind: io::ErrorKind) -> io::Error { io::Error::new(kind, "private OS diagnostic") }

        fn exit_status() -> ExitStatus {
            let mut process = child("exit-7");
            process.child.wait().unwrap()
        }

        #[test]
        fn cleanup_vectors_cover_natural_exit_kill_and_exit_races() {
            let status = exit_status();
            let policy = RedactionPolicy::default();
            let mut exited = ScriptedChild::new(vec![Ok(Some(status))], vec![], vec![]);
            assert_eq!(stop_child(&mut exited, &policy).unwrap(), status);
            exited.assert_consumed(&["try_wait"]);
            let mut killed = ScriptedChild::new(vec![Ok(None)], vec![Ok(())], vec![Ok(status)]);
            assert_eq!(stop_child(&mut killed, &policy).unwrap(), status);
            killed.assert_consumed(&["try_wait", "kill", "wait"]);
            let mut race = ScriptedChild::new(vec![Ok(None), Ok(Some(status))],
                vec![Err(os_error(io::ErrorKind::InvalidInput))], vec![]);
            assert_eq!(stop_child(&mut race, &policy).unwrap(), status);
            race.assert_consumed(&["try_wait", "kill", "try_wait"]);
        }

        #[test]
        fn cleanup_error_vectors_preserve_the_primary_failure_and_allow_retries() {
            let status = exit_status();
            let policy = policy(&["permission denied"]);
            let mut status_error = ScriptedChild::new(
                vec![Err(os_error(io::ErrorKind::Other)), Ok(None)], vec![Ok(())], vec![Ok(status)]);
            assert_eq!(detail(stop_child(&mut status_error, &policy).unwrap_err()),
                "process status check failed: operating system error");
            assert_eq!(stop_child(&mut status_error, &policy).unwrap(), status);
            status_error.assert_consumed(&["try_wait", "try_wait", "kill", "wait"]);
            for secondary in [Ok(None), Err(os_error(io::ErrorKind::Other))] {
                let mut kill_error = ScriptedChild::new(vec![Ok(None), secondary, Ok(Some(status))],
                    vec![Err(os_error(io::ErrorKind::PermissionDenied))], vec![]);
                assert_eq!(detail(stop_child(&mut kill_error, &policy).unwrap_err()),
                    "process termination failed: [REDACTED]");
                assert_eq!(stop_child(&mut kill_error, &policy).unwrap(), status);
                kill_error.assert_consumed(&["try_wait", "kill", "try_wait", "try_wait"]);
            }
            let mut reap_error = ScriptedChild::new(vec![Ok(None), Ok(Some(status))], vec![Ok(())],
                vec![Err(os_error(io::ErrorKind::Other))]);
            assert_eq!(detail(stop_child(&mut reap_error, &policy).unwrap_err()),
                "process reap failed: operating system error");
            assert_eq!(stop_child(&mut reap_error, &policy).unwrap(), status);
            reap_error.assert_consumed(&["try_wait", "kill", "wait", "try_wait"]);
        }

        #[test]
        fn drop_cleanup_vectors_are_best_effort_without_waiting_after_a_failed_kill() {
            let status = exit_status();
            let mut exited = ScriptedChild::new(vec![Ok(Some(status))], vec![], vec![]);
            reap_best_effort(&mut exited);
            exited.assert_consumed(&["try_wait"]);
            for check in [Ok(None), Err(os_error(io::ErrorKind::Other))] {
                let mut killed = ScriptedChild::new(vec![check], vec![Ok(())], vec![Ok(status)]);
                reap_best_effort(&mut killed);
                killed.assert_consumed(&["try_wait", "kill", "wait"]);
            }
            for check in [Ok(Some(status)), Ok(None), Err(os_error(io::ErrorKind::Other))] {
                let mut kill_error = ScriptedChild::new(vec![Ok(None), check],
                    vec![Err(os_error(io::ErrorKind::PermissionDenied))], vec![]);
                reap_best_effort(&mut kill_error);
                kill_error.assert_consumed(&["try_wait", "kill", "try_wait"]);
            }
            let mut reap_error = ScriptedChild::new(vec![Ok(None)], vec![Ok(())],
                vec![Err(os_error(io::ErrorKind::Other))]);
            reap_best_effort(&mut reap_error);
            reap_error.assert_consumed(&["try_wait", "kill", "wait"]);
        }

        struct JsonProbe { reply: Reply, calls: usize }
        impl ReadinessProbe for JsonProbe {
            fn is_ready(&mut self) -> Result<bool> {
                self.calls += 1;
                drive::assert_reply(&self.reply, "ready", &Expectation::JsonEquals(Payload::from_slice(b"true")))?;
                Ok(true)
            }
        }

        #[test]
        fn real_child_readiness_integrates_strict_reply_validation_and_structural_assertions() {
            for body in [br#"{"ready":true,"data":"private-body"}"#.as_slice(),
                br#"{"ready":false}"#, br#"{"ready":true,"ready":true}"#, br#"{"ready":true} trailing"#] {
                let reply = Reply { sequence: None, error_code: None, body: Payload::from_slice(body) };
                let expected = drive::assert_reply(&reply, "ready", &Expectation::JsonEquals(Payload::from_slice(b"true")));
                let mut probe = JsonProbe { reply, calls: 0 };
                let mut process = child("hold");
                match expected {
                    Ok(()) => {
                        assert_eq!(process.poll(&mut probe).unwrap(), ProcessState::Ready);
                        process.terminate().unwrap();
                    }
                    Err(error) => {
                        let actual = process.poll(&mut probe).unwrap_err();
                        assert_eq!(actual, error);
                        assert!(!actual.to_string().contains("private-body"));
                    }
                }
                assert_eq!(probe.calls, 1);
                assert_reaped(&mut process);
            }
        }

        #[test]
        fn drop_cleans_up_live_children_and_is_safe_during_unwinding() {
            struct PanickingProbe;
            impl ReadinessProbe for PanickingProbe {
                fn is_ready(&mut self) -> Result<bool> { panic!("exercise probe unwinding") }
            }
            drop(child("hold"));
            let result = std::panic::catch_unwind(|| {
                let mut process = child("hold");
                let _ = process.poll(&mut PanickingProbe);
            });
            assert!(result.is_err());
        }
    }
}
