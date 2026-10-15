//! Bounded service-driver throughput, latency, and retention measurements.

use alloc::vec::Vec;
use core::{convert::TryFrom, mem::size_of};

use jmax_abi::error::ErrorCode;

use crate::{
    bench_error, corpus, mix_digest, Clock, Completion, Observation, Payload, Result,
    ServiceConfig, ServiceDriver, ServiceReport, Workload,
};

const MAX_SERVICE_REQUESTS: u32 = 1_000_000;

/// Validate service limits and ensure report-tracking sizes are representable.
pub fn validate_service_config(config: ServiceConfig) -> Result<()> {
    if config.max_in_flight == 0
        || config.max_retained_bytes == 0
        || config.requests == 0
        || config.timeout_ns == 0
        || config.requests > MAX_SERVICE_REQUESTS
        || config.max_in_flight > config.requests
    {
        return Err(bench_error(
            ErrorCode::ConstraintViolation,
            "invalid service request, in-flight, retention, or timeout limit",
        ));
    }

    let requests = usize::try_from(config.requests)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "service request count exceeds address space"))?;
    requests.checked_mul(size_of::<u64>())
        .and_then(|bytes| bytes.checked_add(requests.checked_mul(size_of::<u8>())?))
        .and_then(|bytes| bytes.checked_add(requests.checked_mul(size_of::<Observation>())?))
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "service tracking allocation size overflow"))?;
    usize::try_from(config.max_in_flight)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "in-flight count exceeds address space"))?
        .checked_mul(size_of::<u64>())
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "service queue allocation size overflow"))?;
    Ok(())
}

/// Run a bounded asynchronous service workload through the supplied adapter.
pub fn run_service(
    workload: &Workload,
    driver: &mut dyn ServiceDriver,
    config: ServiceConfig,
    clock: &mut dyn Clock,
) -> Result<ServiceReport> {
    validate_service_config(config)?;
    corpus::validate_workload(workload, config.max_retained_bytes)?;

    let mut report = ServiceReport::default();
    report.latencies_ns.try_reserve_exact(config.requests as usize)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, "service latency allocation failed"))?;
    let mut submitted_at = reserve_filled(config.requests as usize, 0u64, "service timestamp allocation failed")?;
    let mut states = reserve_filled(config.requests as usize, 0u8, "service state allocation failed")?;
    let mut observations = reserve_filled(config.requests as usize, None::<Observation>, "service completion allocation failed")?;

    let mut interacted = false;
    let result = run_inner(
        workload,
        driver,
        config,
        clock,
        &mut report,
        &mut submitted_at,
        &mut states,
        &mut observations,
        &mut interacted,
    );

    match result {
        Ok(()) => Ok(report),
        Err(error) => {
            if interacted {
                let _ = checked_cancel(driver, config.max_retained_bytes, &mut report.peak_retained_bytes);
            }
            Err(error)
        }
    }
}

fn run_inner(
    workload: &Workload,
    driver: &mut dyn ServiceDriver,
    config: ServiceConfig,
    clock: &mut dyn Clock,
    report: &mut ServiceReport,
    submitted_at: &mut [u64],
    states: &mut [u8],
    observations: &mut [Option<Observation>],
    interacted: &mut bool,
) -> Result<()> {
    *interacted = true;
    let initial_retained = driver.retained_bytes();
    if initial_retained != 0 {
        return Err(bench_error(ErrorCode::ConstraintViolation, "service driver must start with zero retained bytes"));
    }
    check_retained(initial_retained, config.max_retained_bytes, &mut report.peak_retained_bytes)?;

    let initial_poll = driver.poll_completion();
    let initial_after = driver.retained_bytes();
    let initial_retained_check = check_retained(initial_after, config.max_retained_bytes, &mut report.peak_retained_bytes);
    let initial_completion = initial_poll?;
    initial_retained_check?;
    if initial_completion.is_some() {
        return Err(bench_error(ErrorCode::InternalError, "service driver has a pending completion before submission"));
    }

    let mut timer = Timer::new(clock);
    let start = timer.start;
    let mut next_id = 0u32;
    let mut in_flight = 0u32;

    while report.completed < u64::from(config.requests) {
        timer.check_timeout(config.timeout_ns)?;

        if next_id < config.requests && in_flight < config.max_in_flight {
            let payload_index = (next_id as usize) % workload.payloads.len();
            let payload = workload.payloads[payload_index].clone();
            let payload_bytes = u64::try_from(payload.len())
                .map_err(|_| bench_error(ErrorCode::PayloadTooLarge, "service input length exceeds u64"))?;
            let before = checked_retained(driver, config.max_retained_bytes, &mut report.peak_retained_bytes)?;
            if before.checked_add(payload_bytes)
                .filter(|total| *total <= config.max_retained_bytes)
                .is_none()
            {
                return Err(bench_error(ErrorCode::BudgetExhausted, "service admission exceeds retained-byte budget"));
            }
            let attempt_at = timer.now()?;
            timer.check_elapsed(start, attempt_at, config.timeout_ns)?;

            *interacted = true;
            let submission = driver.submit(u64::from(next_id), payload);
            let after = driver.retained_bytes();
            let retained_check = check_retained(after, config.max_retained_bytes, &mut report.peak_retained_bytes);
            let accepted = submission?;
            retained_check?;
            if accepted {
                states[next_id as usize] = 1;
                submitted_at[next_id as usize] = attempt_at;
                next_id += 1;
                in_flight += 1;
                report.submitted = report.submitted.checked_add(1)
                    .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "submitted request count overflow"))?;
                continue;
            }
            if after != before {
                return Err(bench_error(ErrorCode::ConstraintViolation, "rejected service submission changed retained bytes"));
            }

            // A rejected input was not retained. Poll once to release a credit;
            // if there is no completion, the next loop iteration checks timeout.
            poll_one(
                driver, config, report, states, submitted_at, observations,
                &mut in_flight, &mut timer,
            )?;
            continue;
        }

        if in_flight == 0 {
            return Err(bench_error(ErrorCode::InternalError, "service made no admission or completion progress"));
        }
        poll_one(
            driver, config, report, states, submitted_at, observations,
            &mut in_flight, &mut timer,
        )?;
    }

    let end = timer.now()?;
    timer.check_elapsed(start, end, config.timeout_ns)?;
    report.elapsed_ns = end.checked_sub(start)
        .ok_or_else(|| bench_error(ErrorCode::InternalError, "service clock moved backwards"))?;
    report.digest = 0;
    for observation in observations.iter() {
        let observation = observation.ok_or_else(|| bench_error(ErrorCode::InternalError, "service result is missing"))?;
        report.digest = mix_digest(report.digest, observation.digest);
    }

    checked_cancel(driver, config.max_retained_bytes, &mut report.peak_retained_bytes)?;
    let queued = checked_poll_after_cancel(driver, config.max_retained_bytes, &mut report.peak_retained_bytes)?;
    if queued.is_some() {
        return Err(bench_error(ErrorCode::ConstraintViolation, "service driver retained a completion after cancellation"));
    }
    report.final_retained_bytes = driver.retained_bytes();
    check_retained(report.final_retained_bytes, config.max_retained_bytes, &mut report.peak_retained_bytes)?;
    if report.final_retained_bytes != 0 {
        return Err(bench_error(ErrorCode::ConstraintViolation, "service driver retained bytes after cancellation"));
    }
    if report.submitted != u64::from(config.requests)
        || report.completed != u64::from(config.requests)
        || report.latencies_ns.len() != config.requests as usize
    {
        return Err(bench_error(ErrorCode::InternalError, "service report is incomplete"));
    }
    Ok(())
}

fn poll_one(
    driver: &mut dyn ServiceDriver,
    config: ServiceConfig,
    report: &mut ServiceReport,
    states: &mut [u8],
    submitted_at: &[u64],
    observations: &mut [Option<Observation>],
    in_flight: &mut u32,
    timer: &mut Timer<'_>,
) -> Result<()> {
    timer.check_timeout(config.timeout_ns)?;
    checked_retained(driver, config.max_retained_bytes, &mut report.peak_retained_bytes)?;
    let completion = driver.poll_completion();
    let observed_at = match &completion {
        Ok(Some(_)) => timer.now().map(Some),
        _ => Ok(None),
    };
    let after = driver.retained_bytes();
    let retained_check = check_retained(after, config.max_retained_bytes, &mut report.peak_retained_bytes);
    let completion = completion?;
    let observed_at = observed_at?;
    retained_check?;
    let Some(Completion { id, observation }) = completion else {
        // A quiet poll still consumes time and must respect the whole-run limit.
        timer.check_timeout(config.timeout_ns)?;
        return Ok(());
    };
    let observed_at = observed_at.ok_or_else(|| bench_error(ErrorCode::InternalError, "completion timestamp is missing"))?;
    timer.check_elapsed(timer.start, observed_at, config.timeout_ns)?;
    let index = usize::try_from(id)
        .map_err(|_| bench_error(ErrorCode::InternalError, "completion ID exceeds address space"))?;
    if index >= states.len() || states[index] != 1 {
        return Err(bench_error(ErrorCode::InternalError, "duplicate, unknown, or unadmitted completion ID"));
    }
    let latency = observed_at.checked_sub(submitted_at[index])
        .ok_or_else(|| bench_error(ErrorCode::InternalError, "completion preceded its accepted submission"))?;
    states[index] = 2;
    observations[index] = Some(observation);
    report.latencies_ns.push(latency);
    report.completed = report.completed.checked_add(1)
        .ok_or_else(|| bench_error(ErrorCode::BudgetExhausted, "completed request count overflow"))?;
    *in_flight = in_flight.checked_sub(1)
        .ok_or_else(|| bench_error(ErrorCode::InternalError, "service in-flight count underflow"))?;
    Ok(())
}

fn checked_retained(driver: &dyn ServiceDriver, limit: u64, peak: &mut u64) -> Result<u64> {
    let retained = driver.retained_bytes();
    check_retained(retained, limit, peak)?;
    Ok(retained)
}

fn check_retained(retained: u64, limit: u64, peak: &mut u64) -> Result<()> {
    *peak = (*peak).max(retained);
    if retained > limit {
        return Err(bench_error(ErrorCode::BudgetExhausted, "service driver exceeded retained-byte budget"));
    }
    Ok(())
}

fn checked_cancel(driver: &mut dyn ServiceDriver, limit: u64, peak: &mut u64) -> Result<()> {
    let before = driver.retained_bytes();
    *peak = (*peak).max(before);
    let cancelled = driver.cancel_all();
    let after = driver.retained_bytes();
    *peak = (*peak).max(after);
    if let Err(error) = cancelled { return Err(error); }
    if before > limit || after > limit {
        return Err(bench_error(ErrorCode::BudgetExhausted, "service driver exceeded retained-byte budget around cancellation"));
    }
    Ok(())
}

fn checked_poll_after_cancel(driver: &mut dyn ServiceDriver, limit: u64, peak: &mut u64) -> Result<Option<Completion>> {
    let before = driver.retained_bytes();
    *peak = (*peak).max(before);
    if before > limit {
        return Err(bench_error(ErrorCode::BudgetExhausted, "service driver exceeded retained-byte budget before post-cancel poll"));
    }
    let completion = driver.poll_completion();
    let after = driver.retained_bytes();
    *peak = (*peak).max(after);
    if after > limit {
        return Err(bench_error(ErrorCode::BudgetExhausted, "service driver exceeded retained-byte budget after cancellation"));
    }
    completion
}

fn reserve_filled<T: Clone>(count: usize, value: T, detail: &'static str) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(count)
        .map_err(|_| bench_error(ErrorCode::BudgetExhausted, detail))?;
    values.resize(count, value);
    Ok(values)
}

struct Timer<'a> {
    clock: &'a mut dyn Clock,
    start: u64,
    last: u64,
}

impl<'a> Timer<'a> {
    fn new(clock: &'a mut dyn Clock) -> Self {
        let start = clock.now_ns();
        Self { clock, start, last: start }
    }

    fn now(&mut self) -> Result<u64> {
        let now = self.clock.now_ns();
        if now < self.last {
            return Err(bench_error(ErrorCode::InternalError, "service clock moved backwards"));
        }
        self.last = now;
        Ok(now)
    }

    fn check_elapsed(&self, start: u64, now: u64, timeout: u64) -> Result<()> {
        let elapsed = now.checked_sub(start)
            .ok_or_else(|| bench_error(ErrorCode::InternalError, "service clock moved backwards"))?;
        if elapsed > timeout {
            return Err(bench_error(ErrorCode::BudgetExhausted, "service run exceeded its timeout"));
        }
        Ok(())
    }

    fn check_timeout(&mut self, timeout: u64) -> Result<()> {
        let now = self.now()?;
        self.check_elapsed(self.start, now, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use crate::{CorpusConfig, WorkloadKind};

    struct StepClock { now: u64, step: u64 }
    impl Clock for StepClock {
        fn now_ns(&mut self) -> u64 {
            let value = self.now;
            self.now = self.now.saturating_add(self.step);
            value
        }
    }

    struct ImmediateDriver {
        retained: u64,
        pending: Vec<Completion>,
        reverse: bool,
        leak_after_cancel: bool,
    }

    impl ImmediateDriver {
        fn new(reverse: bool) -> Self {
            Self { retained: 0, pending: Vec::new(), reverse, leak_after_cancel: false }
        }
    }

    impl ServiceDriver for ImmediateDriver {
        fn submit(&mut self, id: u64, input: Payload) -> Result<bool> {
            self.retained = self.retained.saturating_add(input.len() as u64);
            let digest = crate::digest_bytes(input.as_slice());
            self.pending.push(Completion {
                id,
                observation: Observation { digest, output_bytes: 0, retained_bytes: self.retained },
            });
            Ok(true)
        }

        fn poll_completion(&mut self) -> Result<Option<Completion>> {
            let item = if self.reverse { self.pending.pop() } else if self.pending.is_empty() { None } else { Some(self.pending.remove(0)) };
            if let Some(completion) = item {
                self.retained = self.retained.saturating_sub(2);
                Ok(Some(completion))
            } else { Ok(None) }
        }

        fn retained_bytes(&self) -> u64 { self.retained }

        fn cancel_all(&mut self) -> Result<()> {
            self.pending.clear();
            if !self.leak_after_cancel { self.retained = 0; }
            Ok(())
        }
    }

    fn workload() -> Workload {
        corpus::make_workload(WorkloadKind::SmallObject, CorpusConfig {
            seed: 17, documents: 2, items_per_document: 2, max_bytes: 4096,
        }).unwrap()
    }

    fn config() -> ServiceConfig {
        ServiceConfig { max_in_flight: 2, max_retained_bytes: 4096, requests: 5, timeout_ns: 10_000 }
    }

    #[test]
    fn config_boundaries_and_overflow_are_checked() {
        assert!(validate_service_config(config()).is_ok());
        for invalid in [
            ServiceConfig { max_in_flight: 0, ..config() },
            ServiceConfig { requests: 0, ..config() },
            ServiceConfig { max_in_flight: 6, ..config() },
            ServiceConfig { timeout_ns: 0, ..config() },
            ServiceConfig { max_retained_bytes: 0, ..config() },
            ServiceConfig { requests: MAX_SERVICE_REQUESTS + 1, ..config() },
        ] {
            assert_eq!(validate_service_config(invalid).unwrap_err().code, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn service_tracks_all_requests_and_order_independent_digest() {
        let workload = workload();
        let mut clock_a = StepClock { now: 10, step: 1 };
        let mut clock_b = StepClock { now: 10, step: 1 };
        let mut forward = ImmediateDriver::new(false);
        let mut reverse = ImmediateDriver::new(true);
        let a = run_service(&workload, &mut forward, config(), &mut clock_a).unwrap();
        let b = run_service(&workload, &mut reverse, config(), &mut clock_b).unwrap();
        assert_eq!(a.submitted, 5);
        assert_eq!(a.completed, 5);
        assert_eq!(a.latencies_ns.len(), 5);
        assert_eq!(a.final_retained_bytes, 0);
        assert_eq!(a.digest, b.digest);
        assert!(a.latencies_ns.iter().all(|latency| *latency > 0));
    }

    #[test]
    fn rejects_invalid_initial_state_and_budget_before_admission() {
        let workload = workload();
        let mut driver = ImmediateDriver::new(false);
        driver.retained = 1;
        let mut clock = StepClock { now: 0, step: 1 };
        assert_eq!(run_service(&workload, &mut driver, config(), &mut clock).unwrap_err().code, ErrorCode::ConstraintViolation);

        let mut tiny = ImmediateDriver::new(false);
        let limited = ServiceConfig { max_retained_bytes: 1, ..config() };
        assert_eq!(run_service(&workload, &mut tiny, limited, &mut clock).unwrap_err().code, ErrorCode::BudgetExhausted);
        assert_eq!(tiny.retained, 0);
    }

    #[test]
    fn malformed_workload_and_timeout_take_error_paths() {
        let mut bad = workload();
        bad.payloads[0] = Payload::from_slice(b"[}");
        let mut driver = ImmediateDriver::new(false);
        let mut clock = StepClock { now: 0, step: 1 };
        assert!(run_service(&bad, &mut driver, config(), &mut clock).is_err());

        let mut slow = ImmediateDriver::new(false);
        let short = ServiceConfig { timeout_ns: 4, ..config() };
        let mut advancing = StepClock { now: 0, step: 2 };
        assert_eq!(run_service(&workload(), &mut slow, short, &mut advancing).unwrap_err().code, ErrorCode::BudgetExhausted);
        assert_eq!(slow.retained, 0);
    }

    #[test]
    fn cancellation_leak_is_a_constraint_violation() {
        let mut driver = ImmediateDriver::new(false);
        driver.leak_after_cancel = true;
        let mut clock = StepClock { now: 0, step: 1 };
        assert_eq!(run_service(&workload(), &mut driver, config(), &mut clock).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn rejected_submission_retries_same_id_without_retaining_input() {
        struct RejectFirst { inner: ImmediateDriver, reject: bool }
        impl ServiceDriver for RejectFirst {
            fn submit(&mut self, id: u64, input: Payload) -> Result<bool> {
                if self.reject {
                    self.reject = false;
                    return Ok(false);
                }
                self.inner.submit(id, input)
            }
            fn poll_completion(&mut self) -> Result<Option<Completion>> { self.inner.poll_completion() }
            fn retained_bytes(&self) -> u64 { self.inner.retained_bytes() }
            fn cancel_all(&mut self) -> Result<()> { self.inner.cancel_all() }
        }
        let mut driver = RejectFirst { inner: ImmediateDriver::new(false), reject: true };
        let mut clock = StepClock { now: 0, step: 1 };
        let report = run_service(&workload(), &mut driver, config(), &mut clock).unwrap();
        assert_eq!(report.submitted, 5);
        assert_eq!(report.completed, 5);
        assert_eq!(driver.inner.pending.len(), 0);
    }

    #[test]
    fn completion_id_corruption_is_rejected_and_cancelled() {
        struct BadIdDriver { inner: ImmediateDriver }
        impl ServiceDriver for BadIdDriver {
            fn submit(&mut self, id: u64, input: Payload) -> Result<bool> { self.inner.submit(id, input) }
            fn poll_completion(&mut self) -> Result<Option<Completion>> {
                let mut completion = self.inner.poll_completion()?;
                if let Some(value) = completion.as_mut() { value.id = u64::MAX; }
                Ok(completion)
            }
            fn retained_bytes(&self) -> u64 { self.inner.retained_bytes() }
            fn cancel_all(&mut self) -> Result<()> { self.inner.cancel_all() }
        }
        let mut driver = BadIdDriver { inner: ImmediateDriver::new(false) };
        let mut clock = StepClock { now: 0, step: 1 };
        assert_eq!(run_service(&workload(), &mut driver, config(), &mut clock).unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(driver.retained_bytes(), 0);
    }

    #[test]
    fn deterministic_digest_vector_uses_ascending_request_ids() {
        let mut values = vec![None; 3];
        values[0] = Some(Observation { digest: 11, output_bytes: 0, retained_bytes: 0 });
        values[1] = Some(Observation { digest: 22, output_bytes: 0, retained_bytes: 0 });
        values[2] = Some(Observation { digest: 33, output_bytes: 0, retained_bytes: 0 });
        let expected = mix_digest(mix_digest(mix_digest(0, 11), 22), 33);
        let actual = values.iter().fold(0, |state, item| mix_digest(state, item.unwrap().digest));
        assert_eq!(actual, expected);
    }
}
