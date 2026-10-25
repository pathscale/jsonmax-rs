//! Validated boot configuration, literal redaction, and owned child lifecycles.
//!
//! Polling performs no scheduling. The caller supplies a prompt readiness probe
//! and schedules subsequent polls; termination and destruction reap the child.

use super::*;

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
pub trait ReadinessProbe {
    fn is_ready(&mut self) -> Result<bool>;
}

/// The unique owner of a child, its startup deadline, and its cached state.
///
/// The spawned PID remains available after exit. No child streams are inherited
/// or piped. Dropping the owner attempts to kill and reap any remaining child.
pub struct ManagedProcess {
    child: Child,
    pid: u32,
    deadline: Instant,
    state: ProcessState,
    policy: RedactionPolicy,
}

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

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        if !matches!(self.state, ProcessState::Exited { .. }) {
            // This path does not allocate diagnostics, invoke probes, or panic
            // on OS failures. In particular it also works during unwinding.
            reap_best_effort(&mut self.child);
        }
    }
}

fn startup_deadline(now: Instant, timeout: Duration) -> Result<Instant> {
    now.checked_add(timeout)
        .ok_or_else(|| process_error("startup deadline exceeds monotonic clock range"))
}

// OS error Display implementations can contain paths or custom body contents.
// Select a fixed error classification instead, then apply the caller's policy
// to the whole diagnostic. No arguments, environment, or probe data are read.
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
trait ChildLifecycle {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
}

impl ChildLifecycle for Child {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> { Child::try_wait(self) }
    fn kill(&mut self) -> io::Result<()> { Child::kill(self) }
    fn wait(&mut self) -> io::Result<ExitStatus> { Child::wait(self) }
}

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
